#Requires -Version 5.1
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Directory)

$ErrorActionPreference = 'Stop'
$version = '6.7.4'
$archive = "innosetup-$version.exe"
$directory = [IO.Path]::GetFullPath($Directory)
New-Item -ItemType Directory -Path $directory -Force | Out-Null
$setup = Join-Path $directory $archive
$compiler = Join-Path $directory 'compiler'
Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/jrsoftware/issrc/releases/download/is-6_7_4/$archive" -OutFile $setup
$signature = Get-AuthenticodeSignature -LiteralPath $setup
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '^CN=Pyrsys B\.V\.,') {
    throw 'The Inno Setup compiler signature is invalid.'
}
$arguments = @('/PORTABLE=1', '/CURRENTUSER', '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NOICONS',
    '/NORESTART', ('/DIR="{0}"' -f $compiler))
$process = Start-Process -FilePath $setup -WindowStyle Hidden -Wait -PassThru -ArgumentList $arguments
if ($process.ExitCode -ne 0) { throw "Inno Setup extraction failed ($($process.ExitCode))." }
(Get-Item -LiteralPath (Join-Path $compiler 'ISCC.exe')).FullName
