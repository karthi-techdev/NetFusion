# ==============================================================================
# NetFusion Windows Client Service Management Script
# ==============================================================================

param (
    [Parameter(Mandatory=$false)]
    [ValidateSet("install", "uninstall", "start", "stop", "status", "restart")]
    [string]$Action = "status",

    [string]$InstallDir = "$env:ProgramFiles\NetFusion"
)

$ServiceName = "NetFusionService"
$DisplayName = "NetFusion Internet Bonding Engine"
$BinaryName = "netfusion-client.exe"

function Check-Admin {
    $currentPrincipal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Write-Error "This script requires Administrator privileges. Please re-run PowerShell as Administrator."
        exit 1
    }
}

switch ($Action) {
    "install" {
        Check-Admin
        Write-Host "Installing $DisplayName into $InstallDir..." -ForegroundColor Cyan

        # Create target directory
        if (-not (Test-Path $InstallDir)) {
            New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        }

        # Copy client binary
        $sourceExe = "$PSScriptRoot\..\target\release\$BinaryName"
        if (-not (Test-Path $sourceExe)) {
            $sourceExe = "$PSScriptRoot\..\target\debug\$BinaryName"
        }
        if (Test-Path $sourceExe) {
            Copy-Item -Path $sourceExe -Destination "$InstallDir\$BinaryName" -Force
            Write-Host "Copied $BinaryName to $InstallDir" -ForegroundColor Green
        } else {
            Write-Warning "Could not find compiled $BinaryName. Ensure cargo build has been run."
        }

        # Copy Wintun DLL
        $wintunDll = "$PSScriptRoot\..\drivers\wintun\bin\amd64\wintun.dll"
        if (Test-Path $wintunDll) {
            Copy-Item -Path $wintunDll -Destination "$InstallDir\wintun.dll" -Force
            Write-Host "Copied wintun.dll to $InstallDir" -ForegroundColor Green
        }

        # Copy Config
        $configSrc = "$PSScriptRoot\..\configs\client.json"
        if (Test-Path $configSrc) {
            if (-not (Test-Path "$InstallDir\client.json")) {
                Copy-Item -Path $configSrc -Destination "$InstallDir\client.json" -Force
                Write-Host "Copied default client.json to $InstallDir" -ForegroundColor Green
            }
        }

        # Create Windows Service
        $binPath = "`"$InstallDir\$BinaryName`" --config `"$InstallDir\client.json`" --service-run"
        $existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue

        if ($null -eq $existing) {
            New-Service -Name $ServiceName -BinaryPathName $binPath -DisplayName $DisplayName -StartupType Automatic | Out-Null
            Write-Host "Service $ServiceName created successfully." -ForegroundColor Green
        } else {
            Write-Host "Service $ServiceName is already registered." -ForegroundColor Yellow
        }

        # Start Service
        Start-Service -Name $ServiceName
        Write-Host "Service $ServiceName started." -ForegroundColor Green
    }

    "uninstall" {
        Check-Admin
        Write-Host "Uninstalling $ServiceName..." -ForegroundColor Yellow
        $existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
        if ($existing) {
            if ($existing.Status -eq "Running") {
                Stop-Service -Name $ServiceName -Force
            }
            # Remove service
            & sc.exe delete $ServiceName | Out-Null
            Write-Host "Service $ServiceName removed." -ForegroundColor Green
        } else {
            Write-Host "Service $ServiceName was not found." -ForegroundColor Yellow
        }
    }

    "start" {
        Check-Admin
        Start-Service -Name $ServiceName
        Get-Service -Name $ServiceName
    }

    "stop" {
        Check-Admin
        Stop-Service -Name $ServiceName -Force
        Get-Service -Name $ServiceName
    }

    "restart" {
        Check-Admin
        Restart-Service -Name $ServiceName -Force
        Get-Service -Name $ServiceName
    }

    "status" {
        $svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
        if ($svc) {
            Write-Host "Service Name : $($svc.Name)" -ForegroundColor Cyan
            Write-Host "Display Name : $($svc.DisplayName)" -ForegroundColor Cyan
            Write-Host "Status       : $($svc.Status)" -ForegroundColor ($svc.Status -eq 'Running' ? 'Green' : 'Yellow')
        } else {
            Write-Host "Service $ServiceName is not installed." -ForegroundColor Yellow
            Write-Host "Run: .\service-install.ps1 -Action install" -ForegroundColor DarkGray
        }
    }
}
