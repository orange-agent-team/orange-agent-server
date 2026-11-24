"""Native Windows qualification for the shipped knowledge executable, including inherited NTFS DACLs."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile


def set_acl(path, broad=False):
    # Only invoked for a newly created, unique test directory.
    acl = r"""
$ErrorActionPreference = 'Stop'
$id = [Security.Principal.WindowsIdentity]::GetCurrent().User
$acl = New-Object Security.AccessControl.DirectorySecurity
$acl.SetOwner($id)
$acl.SetAccessRuleProtection($true, $false)
$inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit'
$acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($id,'FullControl',$inherit,'None','Allow'))
if ($env:ORANGE_QA_BROAD -eq '1') {
    $everyone = [Security.Principal.SecurityIdentifier]::new('S-1-1-0')
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($everyone,'Read',$inherit,'None','Allow'))
}
Set-Acl -LiteralPath $env:ORANGE_QA_DIRECTORY -AclObject $acl
"""
    shell = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
    subprocess.run([shell, "-NoProfile", "-NonInteractive", "-Command", acl], check=True, timeout=30,
                   env={**os.environ, "ORANGE_QA_DIRECTORY": str(path), "ORANGE_QA_BROAD": str(int(broad))})


def main():
    if os.name != "nt":
        raise SystemExit("This test requires native Windows and NTFS")
    here = Path(__file__).resolve().parent
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=here.parent / "bin/orange-knowledge.exe")
    args = parser.parse_args()
    exe = args.binary.resolve(strict=True)
    spec = importlib.util.spec_from_file_location("knowledge_client", here / "check-local-knowledge.py")
    support = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(support)
    with tempfile.TemporaryDirectory(prefix="orange-private-knowledge-") as tmp:
        work, private, authority = (Path(tmp) / name for name in ("workspace", "private", "authority"))
        for directory in (work, private, authority):
            directory.mkdir()
            set_acl(directory)
        document = work / "manual.txt"
        document.write_text("Aurora fixture code 95745", encoding="utf-8")

        def open_peer():
            peer = support.Peer(exe, "reader", work, private, [document.name], persistent=True, authority=authority)
            peer.start()
            return peer

        def denied():
            run = subprocess.run([exe, "--persist", "--authority-dir", authority, "reader", "manuals",
                                  work, private, document.name], input="", text=True, capture_output=True, timeout=60)
            support.require(run.returncode != 0 and not run.stdout.strip(), "Unauthorized Windows store opened")

        set_acl(private, broad=True)
        denied()
        support.require(not list(private.iterdir()), "Broad DACL allowed writing cache files")
        set_acl(private)
        peer = open_peer()
        try:
            hit = peer.search("Aurora")["hits"][0]
        finally:
            peer.close()
        peer = open_peer()
        try:
            support.require(peer.read(hit)["structuredContent"]["data"]["text"] == document.read_text(),
                            "Reopened NTFS citation differs")
        finally:
            peer.close()
        result = subprocess.check_output([exe, "--withdraw-saved", "reader", "manuals", private, authority],
                                         timeout=60, text=True)
        support.require(json.loads(result)["state"] == "withdrawn", "Windows withdrawal failed")
        denied()
        print("PASS native Windows private DACL, broad DACL refusal, reopened citation and withdrawal")


if __name__ == "__main__":
    main()
