[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CodexArguments
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$credentialHelper = Join-Path $HOME ".config\codex\read-azure-credential.ps1"
$codexHome = Join-Path $HOME ".codex"
$codexExecutable = Join-Path $PSScriptRoot "bin\codex.exe"

if (-not (Test-Path -LiteralPath (Join-Path $codexHome "azure.config.toml") -PathType Leaf)) {
    [Console]::Error.WriteLine("codex-dev: Azure profile not found: $codexHome\azure.config.toml")
    exit 1
}
if (-not (Test-Path -LiteralPath $codexExecutable -PathType Leaf)) {
    [Console]::Error.WriteLine("codex-dev: Packaged executable not found: $codexExecutable")
    exit 1
}

$azureApiKey = $null
$loadedAzureApiKey = $false
if ([string]::IsNullOrEmpty($env:AZURE_OPENAI_API_KEY)) {
    if (-not (Test-Path -LiteralPath $credentialHelper -PathType Leaf)) {
        [Console]::Error.WriteLine("codex-dev: Azure credential helper not found: $credentialHelper")
        exit 1
    }

    $azureApiKey = & powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $credentialHelper
    if ($LASTEXITCODE -ne 0) {
        [Console]::Error.WriteLine("codex-dev: Could not retrieve the Azure API key; Codex was not launched.")
        exit 1
    }
    if (
        [string]::IsNullOrWhiteSpace($azureApiKey) -or
        $azureApiKey.Contains("`r") -or
        $azureApiKey.Contains("`n") -or
        $azureApiKey.Contains([string][char]0)
    ) {
        [Console]::Error.WriteLine("codex-dev: Credential password is empty or invalid; Codex was not launched.")
        exit 1
    }

    $env:AZURE_OPENAI_API_KEY = $azureApiKey
    $loadedAzureApiKey = $true
}

try {
    $env:CODEX_HOME = $codexHome
    & $codexExecutable --profile azure @CodexArguments
    exit $LASTEXITCODE
} finally {
    if ($loadedAzureApiKey) {
        Remove-Item Env:AZURE_OPENAI_API_KEY -ErrorAction SilentlyContinue
    }
    $azureApiKey = $null
}
