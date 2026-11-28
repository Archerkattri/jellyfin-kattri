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
    $workspaceVersion = $Matches[1]
    if ($hostPackage[0].version -ne $workspaceVersion) {
        throw "Release host version $($hostPackage[0].version) must match workspace version $workspaceVersion for deb/rpm metadata."
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
    if ($demoPackage.Count -ne 1) {
        throw "Expected one jellyfin-egui-client package; found $($demoPackage.Count)."
    }
    if ($demoPackage[0].version -ne $workspaceVersion) {
        throw "egui client version $($demoPackage[0].version) must match workspace version $workspaceVersion so the About page and reported ClientVersion match the release."
    }
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

    $appImageStart = $linux.IndexOf('  appimage:')
    $packagesStart = $linux.IndexOf('  packages:')
    if ($appImageStart -lt 0 -or $packagesStart -le $appImageStart) {
        throw 'Linux release workflow must retain its AppImage job before deb/rpm packaging.'
    }
    $appImageJob = $linux.Substring($appImageStart, $packagesStart - $appImageStart)
    if ($appImageJob -notmatch 'libxkbcommon-x11-0') {
        throw 'AppImage build must install libxkbcommon-x11 so linuxdeploy can bundle the runtime-loaded library.'
    }

    $flatpakStart = $linux.IndexOf('  flatpak:')
    if ($packagesStart -lt 0 -or $flatpakStart -le $packagesStart) {
        throw 'Linux release workflow must retain its deb/rpm package job before Flatpak.'
    }
    $linuxPackageJob = $linux.Substring($packagesStart, $flatpakStart - $packagesStart)
    if ($linuxPackageJob -notmatch 'packaging/libmpv/fetch\.sh linux-x64' -or
        $linuxPackageJob -notmatch 'tar -xzf "\$\{ARCHIVES\[0\]\}" --strip-components=1 -C target/kattri-libmpv' -or
        $linuxPackageJob -notmatch 'ldconfig -p \| awk' -or
        $linuxPackageJob -notmatch 'libjpeg\.so\.8" && !found' -or
        $linuxPackageJob -notmatch 'patchelf --set-rpath ''\$ORIGIN'' target/kattri-libmpv/libmpv\.so\.2' -or
        $linuxPackageJob -notmatch 'LICENSE\.mpv\.LGPL' -or
        $linuxPackageJob -notmatch 'LICENSE\.libdisplay-info\.MIT' -or
        $linuxPackageJob -notmatch 'target/kattri-libmpv/LICENSE\.libjpeg-turbo') {
        throw 'Linux deb/rpm workflow must stage the verified libmpv archive, private JPEG ABI, and upstream license notices.'
    }
    if ($linuxPackageJob -match 'libmpv2\s*\|\s*libmpv1' -or
        $linuxPackageJob -match 'libmpv\.so\.2\(\)\(64bit\)') {
        throw 'Linux deb/rpm workflow must not rely on a system libmpv provider.'
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
    if ($debMetadata -notmatch 'target/kattri-libmpv/libmpv\.so\.2.*usr/lib/kattri/libmpv\.so\.2' -or
        $debMetadata -notmatch 'target/kattri-libmpv/libjpeg\.so\.8.*usr/lib/kattri/libjpeg\.so\.8' -or
        $debMetadata -notmatch 'usr/share/doc/kattri/licenses/libmpv\.LGPL' -or
        $debMetadata -notmatch 'usr/share/doc/kattri/libmpv-BUILD-INFO\.txt' -or
        $debMetadata -match 'libmpv2\s*\|\s*libmpv1' -or
        $debMetadata -notmatch 'libva2' -or
        $debMetadata -notmatch 'libxkbcommon-x11-0') {
        throw 'Deb metadata must bundle libmpv privately, ship its notices, and declare native runtime dependencies.'
    }
    if (-not $rpmMetadata.Contains('auto-req = "no"') -or
        $rpmMetadata.Contains('"libjpeg.so.8()(64bit)"') -or
        -not $rpmMetadata.Contains('"libudev.so.1()(64bit)"') -or
        -not $rpmMetadata.Contains('"libgcc_s.so.1()(64bit)"') -or
        -not $rpmMetadata.Contains('"libc.so.6()(64bit)"') -or
        -not $rpmMetadata.Contains('"libm.so.6()(64bit)"') -or
        $rpmMetadata -notmatch 'target/kattri-libmpv/libmpv\.so\.2.*?/usr/lib/kattri/libmpv\.so\.2' -or
        $rpmMetadata -notmatch 'target/kattri-libmpv/libjpeg\.so\.8.*?/usr/lib/kattri/libjpeg\.so\.8' -or
        $rpmMetadata -notmatch 'target/kattri-libmpv/LICENSE\.mpv\.LGPL' -or
        $rpmMetadata -notmatch 'target/kattri-libmpv/BUILD-INFO\.txt' -or
        $rpmMetadata -match '(?m)^"libmpv\.so\.2\(\)\(64bit\)"' -or
        $rpmMetadata -notmatch '"libva\.so\.2\(\)\(64bit\)"' -or
        $rpmMetadata -notmatch '"libxkbcommon-x11\.so\.0\(\)\(64bit\)"') {
        throw 'RPM metadata must bundle libmpv privately, ship its notices, and declare native runtime dependencies.'
    }
    # auto-req is disabled to avoid generating a system requirement for the private
    # libjpeg.so.8. Assert the complete direct ELF closure recorded by the pinned
    # libmpv build metadata, plus the native library needs of the application.
    foreach ($rpmElfDependency in @(
        'libudev.so.1', 'libgcc_s.so.1', 'libc.so.6', 'libm.so.6',
        'libass.so.9', 'libz.so.1', 'libva.so.2', 'libva-drm.so.2',
        'libuchardet.so.0', 'libstdc++.so.6', 'libXfixes.so.3', 'libX11.so.6',
        'libasound.so.2', 'libpulse.so.0', 'libdrm.so.2', 'libgbm.so.1',
        'libEGL.so.1', 'libxkbcommon-x11.so.0'
    )) {
        if ($rpmMetadata -notmatch ('"' + [regex]::Escape($rpmElfDependency) + '\(\)\(64bit\)"\s*=\s*"\*"')) {
            throw "RPM explicit ELF dependency closure is missing $rpmElfDependency."
        }
    }
    $mpvLibrarySource = Get-Content 'proofs/mpv-render/src/library.rs' -Raw
    if ($mpvLibrarySource -notmatch 'exe_dir\.join\("\.\./lib/kattri/libmpv\.so\.2"\)' -or
        $mpvLibrarySource -notmatch '(?s)let mut candidates: Vec<PathBuf> =.*?candidates\.extend\(platform_candidates\(\)') {
        throw 'Linux libmpv lookup must prefer the private package path before system library names.'
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
        $appImageScript -notmatch '--icon-file' -or
        $appImageScript -notmatch 'LIBXKBCOMMON_X11=' -or
        $appImageScript -notmatch 'libxkbcommon-x11\.so\.0' -or
        $appImageScript -notmatch '--library "\$LIBXKBCOMMON_X11"' -or
        $appImageScript -notmatch 'BUILD-INFO\.txt' -or
        $appImageScript -notmatch 'LICENSE\.ffmpeg\.LGPL' -or
        -not $appImageScript.Contains('sed ''s/\r$//'' "$DESKTOP" > "$DESKTOP_NORMALIZED"') -or
        -not $appImageScript.Contains('desktop-file-validate "$DESKTOP_NORMALIZED"') -or
        $appImageScript -notmatch 'AppImage does not contain the dynamically loaded libxkbcommon-x11 runtime') {
        throw 'AppImage packaging must bundle its dynamic XKB library and pinned libmpv notices.'
    }
    $installWorkflow = Get-Content '.github/workflows/installation-smoke.yml' -Raw
    $flatpakManifest = Get-Content 'packaging/flatpak/org.kattri.JellyfinKattri.yml' -Raw
    $flatpakSmokeStart = $installWorkflow.IndexOf('  linux-flatpak:')
    $flatpakSmokeEnd = $installWorkflow.Length
    if ($flatpakSmokeStart -lt 0) {
        throw 'Could not isolate Flatpak installation-smoke job.'
    }
    $flatpakSmokeJob = $installWorkflow.Substring($flatpakSmokeStart, $flatpakSmokeEnd - $flatpakSmokeStart)
    if ($flatpakSmokeJob -notmatch [regex]::Escape("KATTRI_SMOKE_EXPECT_SANDBOX_LIB_PATH='/app/lib/libmpv.so.2'") -or
        $flatpakSmokeJob -notmatch 'flatpak run --env=LD_DEBUG=libs org\.kattri\.JellyfinKattri' -or
        $flatpakSmokeJob -match 'KATTRI_SMOKE_TRACE_LOADER=0') {
        throw 'Flatpak installation smoke must trace and assert the exact sandbox /app/lib libmpv payload.'
    }
    $linuxSmoke = Get-Content 'packaging/linux/smoke-launch.sh' -Raw
    if ($linuxSmoke -notmatch 'KATTRI_SMOKE_EXPECT_SANDBOX_LIB_PATH' -or
        -not $linuxSmoke.Contains('awk -v expected="$SANDBOX_LIB_PATH"') -or
        -not $linuxSmoke.Contains('$0 == "calling init: " expected') -or
        $linuxSmoke -notmatch 'Flatpak namespace.*never resolve it on the host' -or
        $linuxSmoke -notmatch 'realpath -e -- "\$library_path"' -or
        $linuxSmoke -notmatch 'realpath -e -- "\$candidate_path"') {
        throw 'Flatpak smoke must match its exact sandbox loader path without weakening host realpath checks.'
    }
    if ($flatpakManifest -notmatch '(?m)^\s*- --env=LD_LIBRARY_PATH=/app/lib\s*$' -or
        $flatpakManifest -notmatch 'install -Dm755 libmpv\.so\.2 /app/lib/libmpv\.so\.2' -or
        $flatpakManifest -match '(?m)^\s*- patchelf\b') {
        throw 'Flatpak must install libmpv in its configured /app/lib search path without depending on patchelf in the SDK.'
    }
    if ($flatpakManifest -notmatch 'WITH_JPEG8=ON' -or
        $flatpakSmokeJob -notmatch 'test -s /app/lib/libjpeg\.so\.8' -or
        $flatpakSmokeJob -notmatch 'test -s /app/share/licenses/libjpeg-turbo/LICENSE\.md' -or
        $flatpakSmokeJob -notmatch 'test -s /app/share/licenses/libjpeg-turbo/README\.ijg' -or
        -not $flatpakSmokeJob.Contains('ldd /app/lib/libmpv.so.2') -or
        -not $flatpakSmokeJob.Contains('libjpeg\.so\.8 => /app/lib/libjpeg\.so\.8') -or
        -not $flatpakSmokeJob.Contains('grep -q "not found"; then exit 1')) {
        throw 'Flatpak must verify its bundled JPEG 8 ABI, notices, and libmpv dependency resolution.'
    }
    $flatpakRelease = (Get-Content '.github/workflows/release-linux.yml' -Raw).Split(@('  flatpak:'), 2, [StringSplitOptions]::None)[-1]
    $jpegPin = [regex]::Match($flatpakManifest, '(?m)^\s+url: (https://github\.com/libjpeg-turbo/[^\r\n]+)\r?\n\s+sha256: ([a-f0-9]{64})\s*$')
    if (-not $jpegPin.Success -or
        -not $flatpakRelease.Contains($jpegPin.Groups[1].Value) -or
        -not $flatpakRelease.Contains($jpegPin.Groups[2].Value)) {
        throw 'Flatpak JPEG source URL and digest must match its release evidence.'
    }
    $appImageSmokeLine = @($installWorkflow -split "`n" | Where-Object {
        $_ -match 'apt-get install.*libfuse2' -and $_ -match 'xvfb'
    })
    if ($appImageSmokeLine.Count -ne 1 -or $appImageSmokeLine[0] -match 'libxkbcommon-x11') {
        throw 'AppImage smoke must not install libxkbcommon-x11 on the host; the bundle must provide it.'
    }
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
        $sbomWorkflow -notmatch 'audit-release-executables\.py' -or
        $sbomWorkflow -notmatch '--binary-audit-index dist/sbom/binary-audit-index\.json' -or
        $sbomWorkflow -notmatch 'cargo cyclonedx --manifest-path proofs/egui-gl-host/Cargo\.toml' -or
        $sbomWorkflow -match 'jellyfin-egui-client') {
        throw 'SBOM workflow must keep its source audit distinct, scan exact release executable evidence, and describe the release GL host package.'
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
    $schemeStepStart = $macosWorkflow.IndexOf('name: Verify packaged macOS URL delivery on the ephemeral runner')
    $dmgStepStart = $macosWorkflow.IndexOf('name: Build disk image')
    if ($schemeStepStart -lt 0 -or $dmgStepStart -le $schemeStepStart) {
        throw 'The macOS packaged URL check must run after app signing and before the DMG is created.'
    }
    $schemeStep = $macosWorkflow.Substring($schemeStepStart, $dmgStepStart - $schemeStepStart)
    if ($schemeStep -notmatch 'APP_PATH:\s*dist/macos/Jellyfin Kattri\.app' -or
        $schemeStep -notmatch 'PROFILE_NAME:\s*acceptance-macos-url' -or
        $schemeStep -notmatch 'scripts/acceptance/setup_macos_runner_profile\.sh' -or
        $schemeStep -notmatch 'scripts/acceptance/verify_macos_scheme\.sh') {
        throw 'The macOS packaged URL smoke must use the guarded ephemeral-runner profile and verifier.'
    }
    if ($macosScript -notmatch '(?s)<key>CFBundleURLTypes</key>\s*<array>\s*<dict>' -or
        $macosScript -notmatch '(?s)<key>CFBundleURLSchemes</key>\s*<array>\s*<string>jellyfin</string>') {
        throw 'macOS app bundle must register the jellyfin URL scheme.'
    }

    if ($macosWorkflow -notmatch 'codesign --force --deep -s - "dist/macos/Jellyfin Kattri\.app"' -or
        $macosWorkflow -notmatch 'suffix=-ad-hoc' -or
        $macosWorkflow -match 'suffix=-unsigned') {
        throw 'macOS releases without Developer ID credentials must use and label ad-hoc app signing.'
    }

    # The clean-runner smoke checks the exact app-bundle payload and preserves
    # startup diagnostics; verify the complete contract here without running macOS jobs.
    $macosSmokeStart = $installWorkflow.IndexOf('  macos:')
    if ($macosSmokeStart -lt 0) {
        $macosSmokeStart = $installWorkflow.IndexOf('  macos-dmg:')
    }
    $macosSmokeEnd = $installWorkflow.IndexOf('  linux-deb:', $macosSmokeStart)
    if ($macosSmokeStart -lt 0 -or $macosSmokeEnd -le $macosSmokeStart) {
        throw 'Could not isolate macOS installation-smoke job.'
    }
    $macosSmokeJob = $installWorkflow.Substring($macosSmokeStart, $macosSmokeEnd - $macosSmokeStart)
    foreach ($macosPayload in @(
        '$APP/Contents/Resources/LICENSE',
        '$APP/Contents/Resources/libmpv-NOTICE.txt',
        '$APP/Contents/Frameworks/libmpv.2.dylib',
        '$APP/Contents/MacOS/jellyfin-kattri',
        '$APP/Contents/Resources/licenses/libmpv',
        'BUILD-INFO.txt', 'fribidi.LGPL', 'ffmpeg.LGPL', 'harfbuzz.MIT',
        'dav1d.BSD-2-Clause', 'libass.ISC', 'libplacebo.LGPL', 'mpv.LGPL', 'freetype.FTL'
    )) {
        if ($macosSmokeJob -notmatch [regex]::Escape($macosPayload)) {
            throw "macOS installation smoke is missing exact payload assertion: $macosPayload."
        }
    }
    if ($macosSmokeJob -match 'licenses/libmpv/\$\{?file\}?') {
        throw 'macOS installation smoke must enumerate its exact upstream license payload, not only a generic loop variable.'
    }
    foreach ($macosStartupContract in @(
        'require_payload()',
        'Starting mounted DMG app with Launch Services.',
        'remained alive for 10s after launch',
        'was not observed within 60s after open -a',
        'remained alive 15s after the normal quit request',
        'unified-log.txt',
        'PASS: DMG app process started and quit after the normal AppleScript request (no playback/rendering claim).'
    )) {
        if ($macosSmokeJob -notmatch [regex]::Escape($macosStartupContract)) {
            throw "macOS startup smoke is missing diagnostic/behavior contract: $macosStartupContract."
        }
    }
    if ($macosSmokeJob -match 'crash(ed)?' -and $macosSmokeJob -notmatch 'not evidence of a crash') {
        throw 'macOS launch/startup failure must not be mislabeled as a confirmed crash.'
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

    foreach ($requiredWorkflow in @('Release (Windows packaging)', 'Release macOS universal', 'Release Linux')) {
        if ($installWorkflow -notmatch [regex]::Escape($requiredWorkflow)) {
            throw "Installation smoke workflow must follow $requiredWorkflow."
        }
    }
    $macosSmokeStart = $installWorkflow.IndexOf('  macos:')
    $linuxDebStart = $installWorkflow.IndexOf('  linux-deb:', $macosSmokeStart)
    if ($macosSmokeStart -lt 0 -or $linuxDebStart -le $macosSmokeStart) {
        throw 'Could not isolate the macOS installation-smoke job.'
    }
    $macosSmokeJob = $installWorkflow.Substring($macosSmokeStart, $linuxDebStart - $macosSmokeStart)
    if ($macosSmokeJob -notmatch '(?s)uses: actions/checkout@[^\r\n]+\r?\n\s+with:\r?\n\s+ref: \$\{\{ github\.event\.workflow_run\.head_sha \}\}\r?\n\s+persist-credentials: false') {
        throw 'macOS installation smoke must check out the triggering release head SHA without persisting credentials.'
    }

    $universalWorkflow = Get-Content '.github/workflows/universal-installers.yml' -Raw
    $universalJobs = @(
        @{ Start = '  windows-msi:'; End = '  linux-appimage-x64:'; Required = @('needs: [windows-x64, windows-arm64]', 'name: universal-windows-msi', 'JellyfinKattri-*-universal.msi', 'JellyfinKattri-*-universal.msi.sha256', 'if-no-files-found: error') },
        @{ Start = '  linux-run:'; End = '  macos-pkg:'; Required = @('needs: [linux-appimage-x64]', 'name: universal-linux-run', 'dist/*.run', 'if-no-files-found: error', 'sh "$RUN_FILE" --help') },
        @{ Start = '  macos-pkg:'; End = $null; Required = @('target: aarch64-apple-darwin,x86_64-apple-darwin', 'packaging/macos/build-app.sh', 'packaging/macos/build-pkg.sh', 'name: universal-macos-pkg', 'dist-macos/*.pkg', 'if-no-files-found: error') }
    )
    foreach ($universalJob in $universalJobs) {
        $start = $universalWorkflow.IndexOf($universalJob.Start)
        $end = if ($null -ne $universalJob.End) { $universalWorkflow.IndexOf($universalJob.End, $start) } else { $universalWorkflow.Length }
        if ($start -lt 0 -or $end -le $start) {
            throw "Could not isolate universal installer job $($universalJob.Start.Trim())."
        }
        $jobText = $universalWorkflow.Substring($start, $end - $start)
        foreach ($contract in $universalJob.Required) {
            if (-not $jobText.Contains($contract)) {
                throw "Universal installer job $($universalJob.Start.Trim()) is missing contract: $contract"
            }
        }
    }
    if ($universalWorkflow -notmatch '(?s)name: universal-payload-linux-x64.*?path: dist/linux/\*\.AppImage' -or
        $universalWorkflow -notmatch 'universal-payload-windows-x64' -or
        $universalWorkflow -notmatch 'universal-payload-windows-arm64') {
        throw 'Universal installers must preserve architecture payload artifact handoffs.'
    }
    $checkoutCount = [regex]::Matches($installWorkflow, [regex]::Escape('actions/checkout@')).Count
    $nonPersistedCheckoutCount = [regex]::Matches($installWorkflow, [regex]::Escape('persist-credentials: false')).Count
    if (-not $installWorkflow.Contains('actions: read') -or -not $installWorkflow.Contains('contents: read') -or
        $checkoutCount -eq 0 -or $checkoutCount -ne $nonPersistedCheckoutCount) {
        throw 'Cross-run artifact downloads need actions:read, and release-source checkouts must not persist their token.'
    }
    foreach ($requiredCheck in @(
        'github.event.workflow_run.id',
        'Start-Process msiexec.exe -Wait -PassThru',
        'hdiutil attach',
        'codesign --verify --deep --strict',
        'apt-get install',
        'dnf install',
        'flatpak install',
        'xvfb-run',
        # Pinned to a commit SHA, so check the action name only.
        'actions/upload-artifact@'
    )) {
        if ($installWorkflow -notmatch [regex]::Escape($requiredCheck)) {
            throw "Installation smoke workflow is missing required check: $requiredCheck"
        }
    }
    foreach ($installer in @('/i', '/x')) {
        if ($installWorkflow -notmatch [regex]::Escape("@('$installer")) {
            throw 'MSI smoke checks must wait for installer completion and inspect its exit code.'
        }
    }
    if ($installWorkflow -notmatch [regex]::Escape("@('/i', `$msi.FullName, '/qn'") -or
        $installWorkflow -notmatch [regex]::Escape("Start-Process msiexec.exe -Wait -PassThru -ArgumentList")) {
        throw 'MSI smoke checks must run msiexec with /i, /qn and inspect its exit code.'
    }
    foreach ($requiredMemberCheck in @(
        'release-native-components.json',
        'sidecar.package_members',
        'source_commit -cne',
        'windows-${{ matrix.platform }}',
        'Sidecar evidence file is missing or outside the artifact directory',
        'Downloaded MSI hash or size does not match its release sidecar',
        'Installed MSI member differs from sidecar evidence',
        'sidecar-member-comparisons.txt'
    )) {
        if ($installWorkflow -notmatch [regex]::Escape($requiredMemberCheck)) {
            throw "MSI smoke checks are missing sidecar member verification: $requiredMemberCheck"
        }
    }
    if ((git ls-files --stage packaging/linux/smoke-launch.sh) -notmatch '^100755 ') {
        throw 'Linux smoke launcher must be executable in a fresh checkout.'
    }
    foreach ($diagnostic in @('app-stdout.txt', 'app-stderr.txt', 'display-adapters.txt', 'installed-hashes.txt')) {
        if ($installWorkflow -notmatch [regex]::Escape($diagnostic)) {
            throw "Windows startup smoke must preserve $diagnostic for early process failures."
        }
    }
    if ($installWorkflow -notmatch [regex]::Escape('windows-installation-smoke-${{ matrix.platform }}-${{ matrix.scope }}')) {
        throw 'Windows installation artifacts must include both platform and scope to avoid job upload collisions.'
    }
    if ($installWorkflow -match 'usr/bin/jellyfin-kattri') {
        throw 'Linux smoke setup must launch the packaged /usr/bin/kattri binary.'
    }
    if (([regex]::Matches($installWorkflow, 'smoke-launch\.sh /usr/bin/kattri')).Count -ne 2) {
        throw 'deb and rpm smoke jobs must both launch /usr/bin/kattri.'
    }
    # winit loads libxkbcommon-x11 at run time. AppImage must bundle it, so its smoke host
    # intentionally omits the library. Xvfb also needs a window manager and wmctrl to close.
    foreach ($linuxJob in @(
        @{ Label = 'deb'; Anchor = './artifacts/kattri_*.deb'; Library = 'libxkbcommon-x11-0' },
        @{ Label = 'rpm'; Anchor = './artifacts/kattri-*.rpm'; Library = 'libxkbcommon-x11' },
        @{ Label = 'AppImage'; Anchor = 'libfuse2'; Library = '' },
        @{ Label = 'Flatpak'; Anchor = 'apt-get install -y flatpak'; Library = 'openbox' }
    )) {
        $installLine = @($installWorkflow -split "`n" | Where-Object {
            $_.Contains($linuxJob.Anchor) -and $_ -match 'apt-get install|dnf install'
        })
        if ($installLine.Count -ne 1) {
            throw "Could not find exactly one Linux $($linuxJob.Label) install step."
        }
        $requiredPackages = @('openbox', 'wmctrl')
        if (-not [string]::IsNullOrEmpty($linuxJob.Library)) {
            $requiredPackages += $linuxJob.Library
        }
        foreach ($requiredPackage in $requiredPackages) {
            if (-not $installLine[0].Contains($requiredPackage)) {
                throw "Linux $($linuxJob.Label) smoke setup must install $requiredPackage."
            }
        }
    }
    $linuxSmoke = Get-Content 'packaging/linux/smoke-launch.sh' -Raw
    if ($linuxSmoke -notmatch 'xdotool search' -or
        $linuxSmoke -notmatch 'openbox --sm-disable' -or
        $linuxSmoke -notmatch 'wmctrl -i -c' -or
        $linuxSmoke -notmatch 'xdotool windowquit' -or
        $linuxSmoke -notmatch 'xwininfo') {
        throw 'Linux smoke launcher must start a window manager and request a normal close.'
    }
    if ($linuxSmoke -notmatch 'realpath -e --' -or
        $linuxSmoke -notmatch 'calling init: ' -or
        $linuxSmoke -notmatch 'realpath -e -- "\$candidate_path"') {
        throw 'Deb and RPM smoke must normalize loader paths but still verify bundled library initialization.'
    }
    foreach ($requiredAppImagePattern in @(
        'KATTRI_SMOKE_EXPECT_APPIMAGE_MOUNT',
        'AppImage mount verification requires loader tracing',
        [regex]::Escape('calling init: /tmp/\.mount_[^/]+/usr/lib/libmpv\.so\.2$'),
        [regex]::Escape('calling init: /tmp/\.mount_[^/]+/usr/lib/libjpeg\.so\.8$')
    )) {
        if ($linuxSmoke -notmatch $requiredAppImagePattern) {
            throw "Linux smoke launcher is missing its AppImage mount-path check: $requiredAppImagePattern"
        }
    }
    foreach ($appImageLibrary in @(
        @{
            Pattern = 'calling init: /tmp/\.mount_[^/]+/usr/lib/libmpv\.so\.2$'
            Mounted = 'calling init: /tmp/.mount_kattri123/usr/lib/libmpv.so.2'
            Host = 'calling init: /usr/lib/libmpv.so.2'
        },
        @{
            Pattern = 'calling init: /tmp/\.mount_[^/]+/usr/lib/libjpeg\.so\.8$'
            Mounted = 'calling init: /tmp/.mount_kattri123/usr/lib/libjpeg.so.8'
            Host = 'calling init: /usr/lib/libjpeg.so.8'
        }
    )) {
        if (-not $linuxSmoke.Contains("'$($appImageLibrary.Pattern)'")) {
            throw 'Linux smoke launcher is missing its exact AppImage mount regex.'
        }
        if ($appImageLibrary.Mounted -notmatch $appImageLibrary.Pattern -or
            $appImageLibrary.Host -match $appImageLibrary.Pattern) {
            throw 'AppImage loader regex must accept the mounted bundle and reject a host /usr/lib path.'
        }
    }
    $appImageStart = $installWorkflow.IndexOf('  linux-appimage:')
    $flatpakStart = $installWorkflow.IndexOf('  linux-flatpak:', $appImageStart)
    if ($appImageStart -lt 0 -or $flatpakStart -le $appImageStart) {
        throw 'Could not isolate the AppImage installation-smoke job.'
    }
    $appImageSmokeJob = $installWorkflow.Substring($appImageStart, $flatpakStart - $appImageStart)
    if ($appImageSmokeJob -notmatch 'libgl1-mesa-dri libegl1') {
        throw 'AppImage installation smoke must install libEGL.so.1 on its runner.'
    }
    if ($appImageSmokeJob -notmatch 'TMPDIR=/tmp' -or
        $appImageSmokeJob -notmatch 'KATTRI_SMOKE_EXPECT_APPIMAGE_MOUNT=1' -or
        $appImageSmokeJob -match 'KATTRI_SMOKE_EXPECT_LIBMPV_PATH|KATTRI_SMOKE_EXPECT_LIBJPEG_PATH') {
        throw 'AppImage installation smoke must assert its mounted library paths and avoid host /usr/lib exact paths.'
    }
    if (([regex]::Matches($installWorkflow, [regex]::Escape("KATTRI_SMOKE_EXPECT_LIBMPV_PATH='/usr/lib/kattri/libmpv.so.2'")).Count -ne 2) -or
        ([regex]::Matches($installWorkflow, [regex]::Escape("KATTRI_SMOKE_EXPECT_LIBJPEG_PATH='/usr/lib/kattri/libjpeg.so.8'")).Count -ne 2)) {
        throw 'Deb and RPM installation smoke must retain exact installed library-path checks.'
    }
    $runtimeLinuxSmoke = Get-Content 'scripts/runtime-ci/linux_runtime_smoke.sh' -Raw
    if ($runtimeLinuxSmoke -notmatch 'TMPDIR=/tmp' -or
        $runtimeLinuxSmoke -notmatch 'KATTRI_SMOKE_EXPECT_APPIMAGE_MOUNT=1' -or
        $runtimeLinuxSmoke -match 'KATTRI_SMOKE_EXPECT_LIBMPV_PATH=.*/usr/lib/libmpv\.so\.2') {
        throw 'Runtime acceptance must use the shared AppImage mount-path check instead of a host library path.'
    }

    $desktopEntry = Get-Content 'packaging/linux/share/kattri.desktop' -Raw
    if ($desktopEntry -notmatch '(?m)^Exec=kattri %u[ \t]*\r?$' -or
        $desktopEntry -notmatch '(?m)^MimeType=x-scheme-handler/jellyfin;[ \t]*\r?$' -or
        $debMetadata -notmatch 'target/release/jellyfin-kattri' -or
        $debMetadata -notmatch 'usr/bin/kattri' -or
        $rpmMetadata -notmatch 'target/kattri-stripped' -or
        $rpmMetadata -notmatch '"/usr/bin/kattri"' -or
        $rpmMetadata -notmatch 'packaging/linux/share/kattri\.desktop' -or
        $flatpakManifest -notmatch 'command: kattri' -or
        $flatpakManifest -notmatch 'target/release/jellyfin-kattri /app/bin/kattri' -or
        $flatpakManifest -notmatch 'packaging/linux/share/kattri\.desktop' -or
        $appImageScript -notmatch 'install -m 644 "\$DESKTOP_NORMALIZED" "\$APPDIR/usr/share/applications/kattri\.desktop"' -or
        $appImageScript -notmatch 'install -m 755 "\$BINARY" "\$APPDIR/usr/bin/kattri"' -or
        $appImageScript -notmatch '--desktop-file "\$APPDIR/usr/share/applications/kattri\.desktop"') {
        throw 'Linux URI registration must deliver one URL argument through the packaged kattri executable.'
    }

    $wixProtocol = Get-Content 'packaging/windows/wix/kattri.wxs' -Raw
    $msiBuilder = Get-Content 'packaging/windows/build-msi.ps1' -Raw
    $expectedUriCommand = 'Value="&quot;[INSTALLFOLDER]$(var.ExeName)&quot; &quot;%1&quot;"'
    if ($wixProtocol -notmatch '<ComponentRef Id="JellyfinUriProtocolComponent"\s*/>' -or
        $wixProtocol -notmatch 'Root="HKMU"[\s\S]*?Key="Software\\Classes\\jellyfin"' -or
        $wixProtocol -notmatch 'Name="URL Protocol"' -or
        $wixProtocol -notmatch 'Key="Software\\Classes\\jellyfin\\shell\\open\\command"' -or
        $wixProtocol -notmatch [regex]::Escape($expectedUriCommand) -or
        $msiBuilder -notmatch 'wix\\kattri\.wxs') {
        throw 'Windows MSI must register jellyfin:// and pass the quoted URI as one argument.'
    }

    $launchArgs = Get-Content 'proofs/egui-gl-host/src/launch_intent.rs' -Raw
    foreach ($requiredLaunchTest in @(
        'profile_forms_and_local_media_are_preserved',
        'deep_links_cannot_be_combined_with_local_media_or_each_other',
        'play_actions_are_not_launch_intents',
        'quoted_or_encoded_argument_injection_is_rejected_without_echoing_input',
        'malformed_links_and_ambiguous_options_have_sanitized_errors'
    )) {
        if ($launchArgs -notmatch [regex]::Escape("fn $requiredLaunchTest")) {
            throw "Native launch argument regression test is missing: $requiredLaunchTest"
        }
    }
    $launchTestStart = $launchArgs.IndexOf('#[cfg(test)]')
    if ($launchTestStart -lt 0) {
        throw 'Native launch argument tests are missing.'
    }
    $launchParser = $launchArgs.Substring(0, $launchTestStart)
    if ($launchParser -match 'to_string_lossy|format!') {
        throw 'Native launch argument errors must not echo a deep-link URL or argument.'
    }

    $appSource = Get-Content 'proofs/egui-client/src/app.rs' -Raw
    $hostSource = Get-Content 'proofs/egui-gl-host/src/main.rs' -Raw
    $macosLinks = Get-Content 'proofs/egui-gl-host/src/macos_links.rs' -Raw
    foreach ($requiredMacLinkTest in @(
        'accepts_a_valid_review_intent_without_interpreting_playback_actions',
        'rejects_credential_bearing_urls',
        'rejects_oversized_and_malformed_urls_without_echoing_input',
        'gate_allows_only_one_pre_window_event_until_startup_consumes_it',
        'a_command_line_intent_blocks_a_second_startup_url',
        'startup_routing_never_overwrites_an_existing_intent'
    )) {
        if ($macosLinks -notmatch [regex]::Escape("fn $requiredMacLinkTest")) {
            throw "macOS URL-dispatch regression test is missing: $requiredMacLinkTest"
        }
    }
    if ($macosLinks -notmatch 'handleAppleEvent:withReplyEvent:' -or
        $macosLinks -notmatch 'URL_EVENT_CLASS: u32 = u32::from_be_bytes\(\*b"GURL"\)' -or
        $macosLinks -notmatch 'URL_EVENT_ID: u32 = u32::from_be_bytes\(\*b"GURL"\)' -or
        $macosLinks -notmatch 'DIRECT_OBJECT_KEY: u32 = u32::from_be_bytes\(\*b"----"\)' -or
        $macosLinks -notmatch 'paramDescriptorForKeyword: DIRECT_OBJECT_KEY' -or
        $macosLinks -notmatch 'stringValue\(\)' -or
        $macosLinks -notmatch 'len_utf16\(\) > MAX_DEEP_LINK_BYTES' -or
        $macosLinks -notmatch 'text\.len\(\) > MAX_DEEP_LINK_BYTES' -or
        $macosLinks -notmatch 'setEventHandler:' -or
        $macosLinks -notmatch 'send_event\(HostEvent::MacOpenUrl\(intent\)\)' -or
        $macosLinks -notmatch 'catch_unwind\(AssertUnwindSafe' -or
        $macosLinks -match 'application\.delegate\(\)|NSApplicationDelegate|application:openURLs:' -or
        $hostSource -notmatch 'HostEvent::MacOpenUrl\(intent\)' -or
        $hostSource -notmatch 'host\.kattri_app\.apply_launch_intent\(intent\)') {
        throw 'macOS URL dispatch must parse the bounded GURL direct object, reserve/send through the event gate, avoid replacing the AppKit delegate, and reuse the launch-intent flow.'
    }
    $dispatchStart = $hostSource.IndexOf('if let Some(intent) = launch_intent {')
    if ($dispatchStart -lt 0) {
        throw 'Could not locate the host launch-intent dispatch.'
    }
    $dispatchElse = $hostSource.IndexOf('} else {', $dispatchStart)
    $dispatchEnd = $hostSource.IndexOf('// Load the local file after the app exists', $dispatchElse)
    if ($dispatchElse -le $dispatchStart -or $dispatchEnd -le $dispatchElse) {
        throw 'Could not locate the host launch-intent dispatch.'
    }
    $intentBranch = $hostSource.Substring($dispatchStart, $dispatchElse - $dispatchStart)
    $fallbackBranch = $hostSource.Substring($dispatchElse, $dispatchEnd - $dispatchElse)
    if ($intentBranch -notmatch 'kattri_app\.apply_launch_intent\(intent\)' -or
        $intentBranch -match 'resume_saved_sign_in' -or
        $fallbackBranch -notmatch 'kattri_app\.resume_saved_sign_in\(\)') {
        throw 'A link launch must skip saved sign-in resume.'
    }
    $prefillStart = $appSource.IndexOf('fn launch_intent_prefills_review_fields_without_sign_in_or_playback')
    $manualSignInStart = $appSource.IndexOf('fn matching_manual_sign_in_selects_launch_item_once_without_autoplay')
    $retryStart = $appSource.IndexOf('fn failed_manual_auth_keeps_the_item_pending_for_a_same_server_retry')
    if ($prefillStart -lt 0 -or $manualSignInStart -le $prefillStart -or $retryStart -le $manualSignInStart) {
        throw 'Launch intent tests must cover unauthenticated prefill and manual sign-in behavior.'
    }
    $prefillTest = $appSource.Substring($prefillStart, $manualSignInStart - $prefillStart)
    $manualSignInTest = $appSource.Substring($manualSignInStart, $retryStart - $manualSignInStart)
    if ($prefillTest -notmatch 'ConnectionStatus::Disconnected' -or
        $prefillTest -notmatch 'app\.username\.is_empty\(\)' -or
        $prefillTest -notmatch 'app\.password_input\.is_empty\(\)' -or
        $prefillTest -notmatch '!app\.auto_play\.is_pending\(\)' -or
        $prefillTest -notmatch 'app\.pending_playback\.is_none\(\)' -or
        $manualSignInTest -notmatch 'app\.start_connection\(\)' -or
        $manualSignInTest -notmatch 'WorkerEvent::ItemsLoaded' -or
        $manualSignInTest -notmatch '!app\.auto_play\.is_pending\(\)' -or
        $manualSignInTest -notmatch '!app\.playback_start_loading') {
        throw 'Launch intent must wait for manual authentication, select the item, and never auto-play it.'
    }

    Write-Output 'PASS: release targets and clean-runner installation contracts.'
}
finally {
    Pop-Location
}
