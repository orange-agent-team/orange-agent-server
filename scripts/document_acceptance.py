"""Run a named document provider's contract and authenticated platform gates."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
from owned_postgres import database
from connector_browser_fixture import prepare_browser

def run(provider):
    if provider not in ("notion","confluence"):
        raise ValueError("Unknown document provider")
    root=Path(__file__).resolve().parents[1]
    parser=argparse.ArgumentParser()
    parser.add_argument("--database-url-file",type=Path,required=True)
    parser.add_argument("--identity-config",type=Path,required=True)
    parser.add_argument("--web-repo",type=Path)
    args=parser.parse_args()
    browser=prepare_browser(args.web_repo,root,provider.upper())
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
    env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
    cmd=[cargo,"test","--locked","--offline","-p","orange-control-plane","--features",provider+"-fixture-tests"]
    subprocess.run(cmd+["--lib",provider+"::tests"],cwd=root,env=env,check=True)
    if provider=="confluence":
        subprocess.run(cmd+["--lib","document_handles::tests"],cwd=root,env=env,check=True)
    with database(args.database_url_file,"orange_"+provider+"_") as (_,url):
        subprocess.run(cmd+["--test","user_identity",provider+"_consent::"],cwd=root,
            env=dict(env,**browser,ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve())),check=True)
