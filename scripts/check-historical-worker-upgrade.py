#!/usr/bin/env python3.12
"""Run just the real schema-50/51 control stream upgrades in isolated databases."""
import argparse
import os
from pathlib import Path
import subprocess
import sys

from owned_postgres import database

ROOT=Path(__file__).resolve().parents[1]

def main():
    cli=argparse.ArgumentParser()
    cli.add_argument("--database-url-file",type=Path,default=ROOT/".local/database-url")
    cli.add_argument("--schema",type=int,choices=(50,51),required=True)
    args=cli.parse_args()
    flag,old,prefix={50:("ORANGE_TRANSCRIPTION_OBSERVATION_TESTS","ORANGE_ASR_UPGRADE_DATABASE_URL","orange_asr_old_"),
                     51:("ORANGE_AUDIO_GRANT_TESTS","ORANGE_AUDIO_OLD_DATABASE_URL","orange_audio_old_")}[args.schema]
    with database(args.database_url_file,prefix) as (_,url):
        env={k:v for k,v in os.environ.items() if not k.startswith(("ORANGE_","DATABASE_","STRIPE_"))}
        env.update({flag:"1",old:url,"ORANGE_TEST_DATABASE_URL":url,"ORANGE_TEST_HISTORICAL_SCHEMA_ONLY":str(args.schema),"CARGO_NET_OFFLINE":"true"})
        subprocess.run([sys.executable,ROOT/"scripts/check-worker-session.py"],env=env,cwd=ROOT,check=True)
if __name__=="__main__":main()
