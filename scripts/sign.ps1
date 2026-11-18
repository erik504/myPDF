<#
.SYNOPSIS
    Firma myPDF.exe (Authenticode) e controlla la firma.

.DESCRIPTION
    Firma l'eseguibile con signtool del Windows SDK, con SHA-256 e marca temporale (così la
    firma resta valida anche dopo la scadenza del certificato), poi la verifica come fa
    Windows. Con -Upload sostituisce myPDF.exe nella Release di GitHub della versione
    indicata in Cargo.toml e aggiorna l'impronta SHA-256 nelle note della Release.

    Il certificato si sceglie in uno di questi modi:
      -CertName    nome del certificato, per esempio "Open Source Developer, Mario Rossi"
      -Thumbprint  impronta (identificativo) del certificato
      -PfxFile     file .pfx / .p12 con la chiave privata (insieme a -PfxPassword)
    Senza nessuno dei tre, se nell'archivio di Windows c'è un solo certificato di firma del
    codice, si usa quello. Nome e impronta si possono anche mettere nelle variabili
    d'ambiente MYPDF_SIGN_CERT e MYPDF_SIGN_THUMBPRINT.

    Con Certum SimplySign il certificato compare nell'archivio di Windows quando l'app
    SimplySign Desktop è connessa: basta indicarne il nome.

    Codici di uscita: 0 firmato e verificato, 1 errore, 2 firmato ma la firma non è
    attendibile per Windows (per esempio un certificato di prova).

.EXAMPLE
    .\scripts\sign.ps1 -CertName "Open Source Developer, Mario Rossi"

.EXAMPLE
    .\scripts\sign.ps1 -Build -CertName "Open Source Developer, Mario Rossi" -Upload
    Ricompila, firma, verifica e aggiorna la Release su GitHub.
#>
[CmdletBinding()]
param(
    # File da firmare (predefinito: myPDF.exe nella cartella del progetto)
    [string]$File,
    [string]$CertName = $env:MYPDF_SIGN_CERT,
    [string]$Thumbprint = $env:MYPDF_SIGN_THUMBPRINT,
    [string]$PfxFile,
    [string]$PfxPassword,
    # Server della marca temporale (quello di Certum; va bene con qualsiasi certificato)
    [string]$TimestampUrl = 'http://time.certum.pl',
    [switch]$NoTimestamp,
    # Prima ricompila (cargo build --release) e copia l'eseguibile nella cartella del progetto
    [switch]$Build,
    # Dopo la firma sostituisce myPDF.exe nella Release di GitHub (solo se la firma è attendibile)
    [switch]$Upload,
    [string]$SignTool,
    [string]$Repo = 'erik504/myPDF'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not $File) { $File = Join-Path $root 'myPDF.exe' }

function Fail([string]$Message, [int]$Code = 1) {
    Write-Host "Errore: $Message" -ForegroundColor Red
    exit $Code
}

# Programmi esterni (signtool, cargo, gh): in Windows PowerShell 5.1, con 'Stop', una riga
# scritta su stderr fermerebbe lo script. L'esito si legge da $LASTEXITCODE.
function Invoke-Native([scriptblock]$Block) {
    $saved = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $Block } finally { $ErrorActionPreference = $saved }
}

function Find-SignTool {
    if ($SignTool) {
        if (Test-Path $SignTool) { return $SignTool }
        Fail "signtool non trovato in $SignTool"
    }
    # la versione più recente del Windows SDK
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $best = Get-ChildItem -Path $kits -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match '^\d+(\.\d+)+$' -and (Test-Path (Join-Path $_.FullName 'x64\signtool.exe')) } |
        Sort-Object { [version]$_.Name } -Descending |
        Select-Object -First 1
    if ($best) { return Join-Path $best.FullName 'x64\signtool.exe' }
    $cmd = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    Fail 'signtool.exe non trovato: installa il Windows SDK (basta "Windows SDK Signing Tools") oppure indica -SignTool'
}

function Get-CodeSigningCerts {
    foreach ($store in 'Cert:\CurrentUser\My', 'Cert:\LocalMachine\My') {
        Get-ChildItem -Path $store -CodeSigningCert -ErrorAction SilentlyContinue | Where-Object { $_.HasPrivateKey }
    }
}

$tool = Find-SignTool

if ($Build) {
    Write-Host 'Compilazione (cargo build --release)...'
    Push-Location $root
    try {
        Invoke-Native { cargo build --release }
        if ($LASTEXITCODE -ne 0) { Fail 'compilazione non riuscita' }
    } finally {
        Pop-Location
    }
    Copy-Item (Join-Path $root 'target\release\myPDF.exe') $File -Force
}
if (-not (Test-Path $File)) { Fail "file da firmare non trovato: $File (usa -Build per crearlo)" }
$File = (Resolve-Path $File).Path
if ($Upload -and (Split-Path -Leaf $File) -ne 'myPDF.exe') { Fail 'con -Upload il file deve chiamarsi myPDF.exe' }

# --- scelta del certificato ---
$signArgs = @('sign', '/fd', 'SHA256', '/d', 'myPDF', '/du', "https://github.com/$Repo")
if ($PfxFile) {
    if (-not (Test-Path $PfxFile)) { Fail "file del certificato non trovato: $PfxFile" }
    $signArgs += '/f', (Resolve-Path $PfxFile).Path
    if ($PfxPassword) { $signArgs += '/p', $PfxPassword }
    $what = "il certificato in $PfxFile"
} elseif ($Thumbprint) {
    $signArgs += '/sha1', ($Thumbprint -replace '[^0-9A-Fa-f]', '')
    $what = "il certificato con impronta $Thumbprint"
} elseif ($CertName) {
    $signArgs += '/n', $CertName
    $what = "il certificato ""$CertName"""
} else {
    $certs = @(Get-CodeSigningCerts)
    if ($certs.Count -eq 1) {
        $signArgs += '/sha1', $certs[0].Thumbprint
        $what = "l'unico certificato di firma presente: $($certs[0].Subject)"
    } elseif ($certs.Count -eq 0) {
        Fail ("nessun certificato di firma del codice nell'archivio di Windows.`n" +
            "  Con Certum SimplySign: avvia SimplySign Desktop e connettiti, poi riprova.`n" +
            "  Altrimenti indica -CertName, -Thumbprint oppure -PfxFile.")
    } else {
        Write-Host 'Ci sono più certificati di firma del codice: scegline uno con -Thumbprint.'
        $certs | ForEach-Object { Write-Host ("  {0}  {1}  (scade il {2:d})" -f $_.Thumbprint, $_.Subject, $_.NotAfter) }
        Fail 'certificato non indicato'
    }
}
if (-not $NoTimestamp) { $signArgs += '/tr', $TimestampUrl, '/td', 'SHA256' }
$signArgs += $File

# --- firma ---
Write-Host "Firma di $File con $what..."
Invoke-Native { & $tool @signArgs }
if ($LASTEXITCODE -ne 0) { Fail "signtool non è riuscito a firmare (codice $LASTEXITCODE)" }

# --- verifica, con le regole di Windows (/pa = criteri Authenticode) ---
$verifyOut = Invoke-Native { & $tool verify /pa /v $File 2>&1 }
$verified = ($LASTEXITCODE -eq 0)
$sig = Get-AuthenticodeSignature -FilePath $File
$hash = (Get-FileHash -Path $File -Algorithm SHA256).Hash
Write-Host ''
Write-Host "Firmato da:        $($sig.SignerCertificate.Subject)"
Write-Host ("Certificato fino:  {0:d}" -f $sig.SignerCertificate.NotAfter)
if ($sig.TimeStamperCertificate) {
    Write-Host "Marca temporale:   $($sig.TimeStamperCertificate.Subject)"
} else {
    Write-Host 'Marca temporale:   nessuna (la firma scadrà insieme al certificato)' -ForegroundColor Yellow
}
Write-Host "SHA-256:           $hash"
if ($verified) {
    Write-Host 'Firma valida e attendibile per Windows.' -ForegroundColor Green
} else {
    Write-Host "La firma c'è, ma Windows non la considera attendibile (certificato di prova o non riconosciuto):" -ForegroundColor Yellow
    # il messaggio di signtool va a capo: la riga dell'errore e quella dopo
    $lines = @($verifyOut | ForEach-Object { "$_".Trim() } | Where-Object { $_ })
    $at = -1
    for ($k = 0; $k -lt $lines.Count; $k++) { if ($lines[$k] -match 'SignTool Error') { $at = $k; break } }
    if ($at -ge 0) { $reasons = $lines[$at..([Math]::Min($at + 1, $lines.Count - 1))] } else { $reasons = $lines | Select-Object -Last 3 }
    Write-Host ('  ' + ($reasons -join ' '))
}

# --- aggiornamento della Release su GitHub ---
if ($Upload) {
    if (-not $verified) { Fail "la firma non è attendibile: l'eseguibile NON è stato caricato sulla Release" 2 }
    $line = Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $line) { Fail 'versione non trovata in Cargo.toml' }
    $tag = 'v' + $line.Matches[0].Groups[1].Value
    Write-Host "Caricamento su GitHub nella Release $tag..."
    Invoke-Native { gh release upload $tag $File --clobber --repo $Repo }
    if ($LASTEXITCODE -ne 0) { Fail "caricamento sulla Release $tag non riuscito" }
    # l'impronta SHA-256 scritta nelle note della Release
    $body = (Invoke-Native { gh release view $tag --repo $Repo --json body --jq .body }) -join "`n"
    $newBody = [regex]::Replace($body, '(SHA-256 di `myPDF\.exe`:\s*`)[0-9A-Fa-f]{64}(`)', "`${1}$hash`${2}")
    if ($newBody -ne $body) {
        $notes = Join-Path ([IO.Path]::GetTempPath()) "mypdf-release-notes-$PID.md"
        [IO.File]::WriteAllText($notes, $newBody, (New-Object Text.UTF8Encoding $false))
        Invoke-Native { gh release edit $tag --repo $Repo --notes-file $notes }
        $editCode = $LASTEXITCODE
        Remove-Item $notes -ErrorAction SilentlyContinue
        if ($editCode -ne 0) { Fail "eseguibile caricato, ma le note della Release $tag non sono state aggiornate" }
    }
    Write-Host "Release $tag aggiornata con l'eseguibile firmato." -ForegroundColor Green
}

if ($verified) { exit 0 } else { exit 2 }
