param(
    [ValidateSet("host", "kernel", "all")]
    [string]$Target = "all",

    [ValidateSet("dev", "release")]
    [string]$Profile = "dev"
)

$ErrorActionPreference = "Stop"
$cargoArgs = @("build")

if ($Profile -eq "release") {
    $cargoArgs += "--release"
}

function Assert-CargoSuccess {
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
}

if ($Target -eq "host" -or $Target -eq "all") {
    & cargo @cargoArgs
    Assert-CargoSuccess
}

if ($Target -eq "kernel" -or $Target -eq "all") {
    & cargo @cargoArgs --target aarch64-unknown-none --no-default-features --features no_std
    Assert-CargoSuccess
}
