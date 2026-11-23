#!/usr/bin/env python3
"""Check current licensed text offers against a real development Worker and disposable database."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

from owned_postgres import database

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--environment", type=Path, default=ROOT / ".local")
    parser.add_argument("--web-repo", type=Path, help="Also run the complete Chrome-to-model workflow")
    parser.add_argument("--node")
    args = parser.parse_args()
    private = args.environment.resolve(strict=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith("ORANGE_")}
    env.update(ORANGE_TEST_MODEL_RUNTIME_ROOT=str(private / "model-runtime"),
               ORANGE_TEST_USER_CONFIG_FILE=str(private / "identity/user-config.json"),
               ORANGE_TEST_OIDC_CONFIG_FILE=str(private / "identity/admin-config.json"))
    (ROOT / ".local").mkdir(mode=0o700, exist_ok=True)
    with database(private / "database-url", "orange_market_models_") as (_, url):
        env["ORANGE_TEST_DATABASE_URL"] = url
        if args.web_repo is None:
            command = [sys.executable, "scripts/check-trusted-pricing.py"]
        else:
            env["ORANGE_WEB_COMPUTE_QA_PORT"] = "43128"
            command = [sys.executable, "scripts/check-web-compute.py", "--web-repo", str(args.web_repo.resolve())]
            if args.node:
                command.extend(["--node", args.node])
        directory = None
        saw_catalogue = False
        marker = "test input_cases::cpu_cases::pricing_cases::disabled_price_cannot_create_a_new_quote_or_reservation ... ok"
        with subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, text=True) as child:
            for line in child.stdout:
                print(line, end="", flush=True)
                if marker in line:
                    saw_catalogue = True
                prefix = "Private generated-test evidence: "
                if line.startswith(prefix):
                    directory = (ROOT / line[len(prefix):].strip()).resolve(strict=True)
            if child.wait():
                raise RuntimeError("Model offer acceptance failed")
        if args.web_repo is None:
            if not saw_catalogue:
                raise ValueError("Named catalogue test did not run")
        else:
            if directory is None or directory.parent != ROOT / ".local" or not directory.name.startswith("web-compute-"):
                raise ValueError("Localized browser acceptance directory missing")
            receipt = json.loads((directory / "browser-result.json").read_bytes())
            pin = json.loads((ROOT / "config/local-model.lock.json").read_bytes())["model"]
            if receipt.get("reviewed_model_digest") != pin["sha256"] or receipt.get("reviewed_model_license") != pin["license"]:
                raise ValueError("The browser did not verify the pinned weight and license")
            if not receipt["content_denied_after_deletion"] or not receipt["job_id"]:
                raise ValueError("Incomplete model execution and withdrawal")
    print("PASS reviewed development model catalogue and exact current offer")


if __name__ == "__main__":
    main()
