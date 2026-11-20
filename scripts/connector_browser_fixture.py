"""Build and select exactly one privately hosted registered-connector browser driver."""
from pathlib import Path
import subprocess
from acceptance_origins import reserve_origins

def prepare_browser(web, root, family):
    if not web:
        return {}
    drivers={"CN05":("real-pg-connector.mjs","pg-view-evidence"),"CN06":("real-s3-connector.mjs","s3-browser-evidence")}
    drivers['REPOSITORY']=('real-repository-connector.mjs','repository-browser-evidence')
    script,directory=drivers[family]
    reserve_origins(43122)
    web=web.resolve()
    node=Path.home()/".nvm/versions/node/v22.23.2/bin/node"
    if subprocess.check_output([node,"--version"],text=True).strip()!="v22.23.2":
        raise SystemExit("Pinned Node required")
    subprocess.run([node,web/"scripts/check.mjs"],cwd=web,check=True)
    subprocess.run(["python3",web/"scripts/local-tls.py"],cwd=web,check=True)
    evidence=root/".local"/directory
    evidence.mkdir(mode=0o700,parents=True,exist_ok=True)
    return {"ORANGE_"+family+"_BROWSER":str(web/"apps/web/scripts"/script),"ORANGE_CONNECTOR_NODE":str(node),"ORANGE_CONNECTOR_EVIDENCE":str(evidence)}
