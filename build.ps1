param(
  # Webs que pueden manejar el programa; la primera es la que abre el icono.
  [Parameter(Mandatory = $true)]
  [string[]]$Origin,
  # Parte del tiempo que mina para TokenMiner quien cobra directamente, y la cuenta de Kryptex que la recibe.
  [ValidateRange(0, 50)]
  [int]$Comision = 10,
  [ValidatePattern('^krx[A-Za-z0-9]+$')]
  [string]$CuentaComision = 'krxY96J9JD'
)

$ErrorActionPreference = 'Stop'
$origins = @($Origin | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim().TrimEnd('/') } | Where-Object { $_ })
if ($origins.Count -eq 0) { throw 'Falta Origin' }
foreach ($o in $origins) {
  $uri = [Uri]$o
  if ($uri.AbsoluteUri.TrimEnd('/') -ne $o -or ($uri.Scheme -ne 'https' -and $o -ne 'http://localhost:3000')) {
    throw "Origin debe ser un origen HTTPS sin ruta, o http://localhost:3000 para desarrollo: $o"
  }
}

$env:TOKENMINER_ALLOWED_ORIGIN = $origins -join ','
$env:TOKENMINER_COMISION = "$Comision"
$env:TOKENMINER_CUENTA_COMISION = $CuentaComision
# El ejecutable guarda las rutas de las fuentes para los mensajes de error: se cambian por nombres neutros
# para no repartir las carpetas del equipo que compila (la de Cargo y la que contiene este repositorio y sus motores).
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $HOME '.cargo' }
$env:CARGO_ENCODED_RUSTFLAGS = @("--remap-path-prefix=$cargoHome=cargo", "--remap-path-prefix=$(Split-Path $PSScriptRoot)=src") -join [char]0x1f
$targetBase = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $PSScriptRoot 'target' }
Push-Location $PSScriptRoot
try {
  cargo build --release --bin tokenminer-helper
  if ($LASTEXITCODE -ne 0) { throw 'No se compilo el programa local' }

  $env:TOKENMINER_HELPER_PATH = Join-Path $targetBase 'release\tokenminer-helper.exe'
  cargo build --release --bin tokenminer-setup --features bundle
  if ($LASTEXITCODE -ne 0) { throw 'No se compilo el instalador' }
} finally {
  Pop-Location
}

$installer = Join-Path $targetBase 'release\tokenminer-setup.exe'
$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $installer).Hash.ToLowerInvariant()
$public = Join-Path $PSScriptRoot 'dist'
New-Item -ItemType Directory -Force -Path $public | Out-Null
$published = Join-Path $public 'tokenminer-setup.exe'
Copy-Item -LiteralPath $installer -Destination $published -Force
@{
  origin = $origins[0]
  origins = $origins
  feePercent = $Comision
  feeAccount = $CuentaComision
  sha256 = $hash
  version = '0.9.2'
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $public 'native-manifest.json') -Encoding utf8
Write-Output "Paquete preparado para $($origins -join ', ') en $published"
Write-Output "SHA256: $hash"
