function Install-McpRepl {
    param(
        [switch]$Dev
    )

    $Owner = "posit-dev"
    $Repo = "mcp-repl"
    $App = "mcp-repl"

    if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
        throw "unsupported arch: $($env:PROCESSOR_ARCHITECTURE)"
    }

    $target = "x86_64-pc-windows-msvc"
    if ($Dev) {
        $url = "https://github.com/$Owner/$Repo/releases/download/dev/$App-$target.zip"
    } else {
        $url = "https://github.com/$Owner/$Repo/releases/latest/download/$App-$target.zip"
    }

    $tmp = Join-Path $env:TEMP "$App-install"
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null

    $zip = Join-Path $tmp "$App.zip"
    Invoke-WebRequest $url -OutFile $zip
    Expand-Archive $zip -DestinationPath $tmp -Force

    $src = Join-Path $tmp "$App-$target\$App.exe"
    $dest = Join-Path $HOME "bin"
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    Copy-Item $src (Join-Path $dest "$App.exe") -Force

    Write-Host "installed $App to $dest\$App.exe"
    Write-Host "add $dest to PATH if needed"
}
