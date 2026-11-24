$ErrorActionPreference = 'Stop'
try {
    $path = $env:ORANGE_KNOWLEDGE_ACL_PATH
    $sidType = [System.Security.Principal.SecurityIdentifier]
    $caller = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $acl = Get-Acl -LiteralPath $path
    if ($acl.GetOwner($sidType).Value -ne $caller) { exit 1 }
    $raw = [System.Security.AccessControl.RawSecurityDescriptor]::new($acl.GetSecurityDescriptorBinaryForm(), 0)
    if ($null -eq $raw.DiscretionaryAcl) { exit 1 }
    $allowed = @($caller, 'S-1-5-18', 'S-1-5-32-544')
    $privateChildFileRule = $false
    foreach ($ace in $raw.DiscretionaryAcl) {
        if ($ace -isnot [System.Security.AccessControl.CommonAce] -or $ace.IsCallback) { exit 1 }
        if ($ace.AceQualifier -eq 'AccessDenied') { continue }
        if ($ace.AceQualifier -ne 'AccessAllowed' -or $allowed -notcontains $ace.SecurityIdentifier.Value) { exit 1 }
        if ($ace.AceFlags -band [System.Security.AccessControl.AceFlags]::ObjectInherit) { $privateChildFileRule = $true }
    }
    if ([IO.Directory]::Exists($path) -and -not $privateChildFileRule) { exit 1 }
    exit 0
} catch { exit 1 }
