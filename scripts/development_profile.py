"""Names and ports for the repository's default stack or an explicitly isolated fixture."""
import os
import re

INSTANCE=os.environ.get("ORANGE_DEVELOPMENT_INSTANCE","default")
if not re.fullmatch(r"[a-z][a-z0-9-]{0,40}",INSTANCE):
    raise ValueError("Invalid development instance")

def port(name,default):
    if name in os.environ and INSTANCE=="default":
        raise ValueError("Alternate ports require a named development fixture")
    result=int(os.environ.get(name,default))
    if not 1024<=result<=65535:
        raise ValueError("Development service ports must be unprivileged")
    return result

POSTGRES_PORT=port("ORANGE_DEVELOPMENT_POSTGRES_PORT",55432)
IDENTITY_PORT=port("ORANGE_DEVELOPMENT_IDENTITY_PORT",44180)
if POSTGRES_PORT==IDENTITY_PORT:
    raise ValueError("Development resources require separate ports")
IDENTITY_BASE=f"http://127.0.0.1:{IDENTITY_PORT}"
IDENTITY_NAME="orange-agent-identity-dev"+("" if INSTANCE=="default" else "-"+INSTANCE)
