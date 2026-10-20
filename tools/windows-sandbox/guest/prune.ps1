# Deletes files under @ROOT@ that are absent from the sync manifest; build output directories are never entered.
$root = (Resolve-Path -LiteralPath @ROOT@).ProviderPath.TrimEnd('\')
$manifestName = '.contr-sandbox-manifest'
$manifestPath = Join-Path $root $manifestName

$keep = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($line in [IO.File]::ReadAllLines($manifestPath)) {
    [void] $keep.Add($line.Replace('/', '\'))
}
[void] $keep.Add($manifestName)

$outputDirectories = [Collections.Generic.HashSet[string]]::new(
    [string[]] @('.git', '.vs', 'AppPackages', 'bin', 'node_modules', 'obj', 'target', 'TestResults'),
    [StringComparer]::OrdinalIgnoreCase)

$pending = [Collections.Generic.Stack[IO.DirectoryInfo]]::new()
$pending.Push([IO.DirectoryInfo]::new($root))
$removed = 0
while ($pending.Count -gt 0) {
    foreach ($entry in $pending.Pop().GetFileSystemInfos()) {
        if ($entry -is [IO.DirectoryInfo]) {
            if (-not $outputDirectories.Contains($entry.Name)) {
                $pending.Push($entry)
            }
        }
        elseif (-not $keep.Contains($entry.FullName.Substring($root.Length + 1))) {
            $entry.Delete()
            $removed++
        }
    }
}

Remove-Item -LiteralPath $manifestPath
"removed $removed stale files"
