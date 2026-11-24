"""Read the migration watermark without applying migrations or changing the database."""
import shutil
import subprocess
from urllib.parse import unquote,urlsplit

def schema_version(url,env):
    parsed=urlsplit(url)
    pg=dict(env,PGHOST=parsed.hostname,PGPORT=str(parsed.port),PGUSER=unquote(parsed.username),PGPASSWORD=unquote(parsed.password),PGDATABASE=parsed.path[1:])
    psql=shutil.which('psql') or '/opt/homebrew/opt/postgresql@17/bin/psql'
    def scalar(sql):return subprocess.check_output([psql,'-X','-w','-Atq','-v','ON_ERROR_STOP=1','-c',sql],env=pg,text=True).strip()
    if scalar("SELECT to_regclass('public._sqlx_migrations') IS NULL")=='t':return 0
    return int(scalar("SELECT COALESCE(max(version),0) FROM public._sqlx_migrations WHERE success"))
