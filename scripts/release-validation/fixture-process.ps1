[CmdletBinding()]
param(
    [ValidateRange(65, 3600)]
    [int]$DurationSeconds = 75,

    [ValidateRange(8, 128)]
    [int]$MemoryMiB = 32,

    [switch]$Windowed,

    [switch]$CancelFirstClose
)

$ErrorActionPreference = 'Stop'
$bufferLength = [int]($MemoryMiB * 1MB)
$buffer = [Array]::CreateInstance([byte], $bufferLength)
for ($offset = 0; $offset -lt $buffer.Length; $offset += 4096) {
    $buffer[$offset] = 1
}

if ($Windowed) {
    Add-Type -AssemblyName System.Windows.Forms
    $form = [System.Windows.Forms.Form]::new()
    $form.Text = 'Kattri runtime monitor fixture'
    $form.ShowInTaskbar = $false
    $form.StartPosition = [System.Windows.Forms.FormStartPosition]::Manual
    $form.Location = [System.Drawing.Point]::new(-32000, -32000)
    $form.Size = [System.Drawing.Size]::new(1, 1)
    $form.Opacity = 0.05

    $timer = [System.Windows.Forms.Timer]::new()
    $timer.Interval = [Math]::Min(($DurationSeconds * 1000), [int]::MaxValue)
    $timer.Add_Tick({
        $timer.Stop()
        $form.Close()
    }.GetNewClosure())
    $form.Add_Shown({ $timer.Start() }.GetNewClosure())
    if ($CancelFirstClose) {
        $closeAttemptCount = [ref]0
        $form.Add_FormClosing({
            param($sender, $eventArgs)
            $closeAttemptCount.Value++
            if ($closeAttemptCount.Value -eq 1) {
                $eventArgs.Cancel = $true
            }
        }.GetNewClosure())
    }

    try {
        $form.Show()
        [System.Windows.Forms.Application]::Run($form)
    } finally {
        $timer.Dispose()
        $form.Dispose()
    }
} else {
    Start-Sleep -Seconds $DurationSeconds
}
