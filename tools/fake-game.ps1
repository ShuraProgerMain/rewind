# A stand-in for a game: a borderless window over the whole primary display, in front.
# Closes by itself after -Seconds. Usage: fake-game.ps1 -Title "Fake Game" -Seconds 10
param([string]$Title = 'Fake Game', [int]$Seconds = 10)
Add-Type -AssemblyName System.Windows.Forms
$f = New-Object System.Windows.Forms.Form
$f.Text = $Title
$f.FormBorderStyle = 'None'
$f.StartPosition = 'Manual'
$f.Bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$f.BackColor = [System.Drawing.Color]::FromArgb(24, 40, 64)
$t = New-Object System.Windows.Forms.Timer
$t.Interval = $Seconds * 1000
$t.Add_Tick({ $f.Close() })
$t.Start()
$f.Add_Shown({ $f.Activate() })
[void]$f.ShowDialog()
