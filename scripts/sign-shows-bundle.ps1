#Requires -Version 7.0
param([Parameter(Mandatory)][string]$BundleDirectory)
$ErrorActionPreference = 'Stop'
$expected = 'FE35227FED82F2429AFE763A36358ADDD79C7A4F'
if (-not $env:AT_TERMINAL_PFX -or -not $env:AT_TERMINAL_PFX_PASSWORD) { throw 'The home code-signing identity is not configured' }
$certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new(
  [Convert]::FromBase64String($env:AT_TERMINAL_PFX), $env:AT_TERMINAL_PFX_PASSWORD,
  [Security.Cryptography.X509Certificates.X509KeyStorageFlags]::EphemeralKeySet)
try {
  if ($certificate.Thumbprint -ne $expected -or -not $certificate.HasPrivateKey) { throw 'Unexpected home signing certificate' }
  if ($env:CI -eq 'true') {
    # The hosted runner starts without the public trust installed on home PCs.
    # Only the already pinned public identity is added; its private key stays ephemeral.
    $public = [Security.Cryptography.X509Certificates.X509Certificate2]::new($certificate.Export([Security.Cryptography.X509Certificates.X509ContentType]::Cert))
    foreach ($name in @('Root','TrustedPeople')) {
      $store = [Security.Cryptography.X509Certificates.X509Store]::new($name, 'CurrentUser')
      try { $store.Open('ReadWrite'); $store.Add($public) } finally { $store.Close() }
    }
    $public.Dispose()
  }
  foreach ($name in @('shows-desktop.exe','shows-remote.exe','libmpv-2.dll')) {
    $file = Join-Path $BundleDirectory $name
    $signature = Set-AuthenticodeSignature -FilePath $file -Certificate $certificate -HashAlgorithm SHA256
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Thumbprint -ne $expected) { throw "Signing verification failed for $name`: $($signature.Status)" }
    Write-Host "Signed and verified $name"
  }
} finally { $certificate.Dispose() }
