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
    if provider=="notion":parser.add_argument("--schema47-source-archive",type=Path,help="Digest-pinned source export for a checkout without the old commit")
    args=parser.parse_args()
    rustup=shutil.which("rustup") or "/opt/homebrew/opt/rustup/bin/rustup"
    cargo=Path(subprocess.check_output([rustup,"which","--toolchain","1.97.1","cargo"],text=True).strip())
    legacy={}
    if provider=="notion":
        from historical_notion_api import binary
        legacy["ORANGE_TEST_SCHEMA47_USER_API"]=str(binary(root,cargo,args.schema47_source_archive))
    browser=prepare_browser(args.web_repo,root,provider.upper())
    env=dict(os.environ,PATH=str(cargo.parent)+os.pathsep+os.environ["PATH"])
    cmd=[cargo,"test","--locked","--offline","-p","orange-control-plane","--features",provider+"-fixture-tests"]
    subprocess.run(cmd+["--lib",provider+"::tests"],cwd=root,env=env,check=True)
    if provider=="confluence":
        subprocess.run(cmd+["--lib","document_handles::tests"],cwd=root,env=env,check=True)
    with database(args.database_url_file,"orange_"+provider+"_") as (_,url):
        selected=cmd+["--test","user_identity",provider+"_consent::"]
        scoped=dict(env,**browser,**legacy,ORANGE_TEST_DATABASE_URL=url,ORANGE_TEST_USER_CONFIG_FILE=str(args.identity_config.resolve()))
        if provider=="notion":
            markers={"test notion_consent::notion_egress_grants_and_revoke_during_source_io ... ok",
                     "Verified genuine schema-47 Run opt-in, changed-flag conflict and clean producer exit before migration"}
            with subprocess.Popen([*selected,"--","--show-output"],cwd=root,env=scoped,stdout=subprocess.PIPE,text=True) as child:
                for line in child.stdout:
                    print(line,end="",flush=True);markers.difference_update([m for m in markers if m in line])
                if child.wait() or markers:raise RuntimeError("Missing historical producer or current Notion acceptance")
        else:subprocess.run(selected,cwd=root,env=scoped,check=True)
