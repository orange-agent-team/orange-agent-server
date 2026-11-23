#!/usr/bin/env python3
"""Actual localhost mTLS/gRPC and PostgreSQL. Ephemeral test CA, no public enrollment."""
import os
import json
import re
import sys
from pathlib import Path
import secrets
import shutil
import subprocess
import tempfile
ROOT=Path(__file__).resolve().parents[1]
rustup=shutil.which('rustup') or '/opt/homebrew/opt/rustup/bin/rustup'
cargo=Path(subprocess.check_output([rustup,'which','--toolchain','stable','cargo'],text=True).strip())
environment=os.environ.copy();environment.setdefault("RUST_TEST_THREADS","2");environment['PATH']=str(cargo.parent)+os.pathsep+environment.get('PATH','')
if environment.get('ORANGE_JOB_QUERY_TESTS') and any(environment.get(flag) for flag in ['ORANGE_WITHDRAW_TESTS','ORANGE_REAL_WORKER_PROFILE','ORANGE_WEB_COMPUTE_TESTS','ORANGE_WORKER_HOST_TESTS','ORANGE_PRICED_WORKER_TESTS','ORANGE_WORKER_OUTPUT_TESTS','ORANGE_WORKER_RENEWAL_TESTS','ORANGE_CLEANUP_PROTOCOL_TESTS','ORANGE_CLEANUP_REAL_TESTS','ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS']):
    raise SystemExit('Job metadata checks cannot be combined with another Worker test selector')
if environment.get('ORANGE_QUEUE_EXPIRY_TESTS') and any(environment.get(flag) for flag in ['ORANGE_WITHDRAW_TESTS','ORANGE_JOB_QUERY_TESTS','ORANGE_REAL_WORKER_PROFILE','ORANGE_WEB_COMPUTE_TESTS','ORANGE_WORKER_HOST_TESTS','ORANGE_PRICED_WORKER_TESTS','ORANGE_WORKER_OUTPUT_TESTS','ORANGE_WORKER_RENEWAL_TESTS','ORANGE_CLEANUP_PROTOCOL_TESTS','ORANGE_CLEANUP_REAL_TESTS','ORANGE_CANCEL_PROTOCOL_TESTS','ORANGE_CANCEL_REAL_TESTS']):
    raise SystemExit('Queue-expiration acceptance cannot be combined with another Worker selector')
if environment.get('ORANGE_WEB_COMPUTE_TESTS') and not environment.get('ORANGE_REAL_WORKER_PROFILE'):
    raise SystemExit('Browser/model acceptance requires the actual pinned model profile')
if not environment.get('ORANGE_TEST_DATABASE_URL'):
    environment['ORANGE_TEST_DATABASE_URL']=(ROOT/'.local/database-url').read_text().strip()
openssl='/opt/homebrew/opt/openssl@3/bin/openssl'
if not Path(openssl).is_file():
    openssl=shutil.which('openssl')
if not openssl:raise SystemExit('OpenSSL required for isolated test certificates')
with tempfile.TemporaryDirectory(prefix='orange-worker-mtls-') as directory:
    private=Path(directory);private.chmod(0o700)
    def command(*arguments):
        subprocess.run([openssl,*arguments],cwd=private,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,check=True)
    for ca in ['ca','other-ca']:
        command('req','-x509','-newkey','ec','-pkeyopt','ec_paramgen_curve:P-256','-nodes','-batch','-days','1','-subj','/CN=Orange-'+ca,'-keyout',ca+'.key','-out',ca+'.pem','-addext','basicConstraints=critical,CA:TRUE','-addext','keyUsage=critical,keyCertSign,cRLSign')
    for name in ['server','client-a','client-b','client-c','client-d','client-e','client-f','client-g','client-h','client-i','client-j','client-k','client-l','client-m','client-n','client-o','client-p','client-q','client-r','client-s','client-t','client-u','client-v','client-w','client-x','client-y','client-z','client-aa','client-ab','client-ac','client-ad','client-ae','client-af','client-ag','client-ah','client-ai','client-aj','client-ak','client-al','client-am','client-an','client-ao','client-ap','client-aq','client-ar','client-as','client-at','client-au','client-av','client-aw','client-ax','client-ay','client-az','client-ba','client-bb','client-bc','client-bd','client-be','client-bf','client-bg','client-bh','client-bi','client-bj','client-bk','client-bl','client-bm','client-bn','client-bo','client-bp','client-bq','client-br','client-bs','client-bt','client-bu','client-bv','client-bw','client-bx','client-by','client-bz','client-ca','client-cb','client-cc','client-cd','client-ce','client-cf','client-cg','client-ch','client-ci','client-cj','client-ck','client-cl','client-cm','client-cn','client-co','client-cp','client-cq','client-cr','client-cs','client-ct','client-cu','client-cv','client-cw','client-cx','client-cy','client-da','client-db','client-dc','client-dd','client-de','client-df','client-dg','client-dh','client-di','client-dj','client-dk','client-dl','client-dm','client-dn','client-do','client-dp','client-dq','client-dr','client-ds','client-dt','client-du','client-dv','client-dw','client-dx','client-ea','client-eb','client-ec','client-ed','client-ee','client-ef','client-eg','client-eh','client-ei','client-ej','client-ek','client-el','unregistered','wrong-ca','client-quarantined-heartbeat','client-history-submitter','client-history-other','client-history-socket','client-typed-job','client-job-revision','client-browser-version-personal','client-browser-version-team','client-hello-matrix','client-unregistered-hello','client-unavailable-renewal','client-isolation-first','client-isolation-other','client-isolation-wait']:
        command('req','-new','-newkey','ec','-pkeyopt','ec_paramgen_curve:P-256','-nodes','-batch','-subj','/CN=orange-'+name,'-keyout',name+'.key','-out',name+'.csr')
        extensions='basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage='+('serverAuth' if name=='server' else 'clientAuth')+'\n'
        if name=='server':extensions+='subjectAltName=DNS:localhost,IP:127.0.0.1\n'
        (private/'leaf.ext').write_text(extensions)
        ca='other-ca' if name=='wrong-ca' else 'ca'
        command('x509','-req','-in',name+'.csr','-CA',ca+'.pem','-CAkey',ca+'.key','-set_serial',str(secrets.randbits(128)+1),'-days','1','-extfile','leaf.ext','-out',name+'.pem')
        command('x509','-in',name+'.pem','-outform','DER','-out',name+'.der')
    for file in private.iterdir():file.chmod(0o600)
    environment['ORANGE_WORKER_TLS_DIR']=str(private)
    environment.setdefault('ORANGE_TEST_USER_CONFIG_FILE',str(ROOT/'.local/identity/user-config.json'))
    # Financial recovery uses a second, independently configured Admin identity in the same development deployment.
    environment.setdefault('ORANGE_TEST_OIDC_CONFIG_FILE',str(Path(environment['ORANGE_TEST_USER_CONFIG_FILE']).with_name('admin-config.json')))
    if environment.get('ORANGE_TEST_ISOLATION_PACKAGE_BIN'):
        if not environment.get('ORANGE_WORKER_HOST_TESTS') or not environment.get('ORANGE_REAL_WORKER_PROFILE'):
            raise ValueError('The extracted binary selector requires the full host suite')
        bin_dir=Path(environment['ORANGE_TEST_ISOLATION_PACKAGE_BIN']).resolve(strict=True)
        for name,var in [('orange-worker','ORANGE_WORKER_BINARY'),('orange-user-api','ORANGE_USER_API_BINARY')]:
            exe=bin_dir/name
            if not exe.is_file() or exe.is_symlink() or not os.access(exe,os.X_OK):
                raise ValueError('Missing installed program: '+name)
            environment[var]=str(exe)
    elif environment.get('ORANGE_REAL_WORKER_PROFILE'):
        subprocess.run([str(cargo),'build','-p','orange-worker','--bin','orange-worker','--locked'],cwd=ROOT,env=environment,check=True)
        if environment.get('CARGO_BUILD_TARGET'):raise SystemExit('Actual Worker process tests require a native host build')
        metadata=json.loads(subprocess.check_output([str(cargo),'metadata','--no-deps','--format-version','1','--locked'],cwd=ROOT,env=environment,text=True))
        environment['ORANGE_WORKER_BINARY']=str(Path(metadata['target_directory'])/'debug/orange-worker')
        if environment.get('ORANGE_WORKER_HOST_TESTS') or environment.get('ORANGE_PRICED_WORKER_TESTS'):
            subprocess.run([str(cargo),'build','-p','orange-control-plane','--bin','orange-user-api','--locked'],cwd=ROOT,env=environment,check=True)
            environment['ORANGE_USER_API_BINARY']=str(Path(metadata['target_directory'])/'debug/orange-user-api')
    if environment.get('ORANGE_QUEUE_EXPIRY_TESTS'):
        subprocess.run([str(cargo),'build','-p','orange-control-plane','--bin','orange-user-api','--locked'],cwd=ROOT,env=environment,check=True)
        if environment.get('CARGO_BUILD_TARGET'):raise SystemExit('Actual API maintenance test requires a native build')
        metadata=json.loads(subprocess.check_output([str(cargo),'metadata','--no-deps','--format-version','1','--locked'],cwd=ROOT,env=environment,text=True))
        environment['ORANGE_USER_API_BINARY']=str(Path(metadata['target_directory'])/'debug/orange-user-api')
    features='queue-expiry-tests' if environment.get('ORANGE_QUEUE_EXPIRY_TESTS') else ('web-compute-tests' if environment.get('ORANGE_WEB_COMPUTE_TESTS') else ('real-worker-tests' if environment.get('ORANGE_REAL_WORKER_PROFILE') else 'integration-tests'))
    if environment.get('ORANGE_AUDIO_GRANT_TESTS'):
        features='transcription-input-tests'
    elif environment.get('ORANGE_TRANSCRIPTION_OBSERVATION_TESTS'):
        features='transcription-observation-tests'
    if environment.get("ORANGE_JOB_REVISION_BROWSER_TEST"): features="job-revision-browser-tests"
    command=[str(cargo),'test','-p','orange-control-plane','--features',features,'--test','worker_sessions','--locked']
    if environment.get('ORANGE_ISOLATION_REPORT_TESTS'):
        command+=['isolation_observations::','--','--test-threads=1']
    elif environment.get('ORANGE_CAPABILITY_OFFER_TEST'):
        command+=['rejected_capability_offers_do_not_rotate_the_current_device_session','--','--exact']
    elif environment.get('ORANGE_REAL_WORKER_PROFILE') and environment.get('ORANGE_UNAVAILABLE_RENEWAL_TEST'):
        command+=['actual_lost_renewal_response_stops_owned_cpu_before_any_result_is_published']
    elif environment.get('ORANGE_EXECUTION_BARRIER_TESTS'):
        command+=['execution_cases::','--','--test-threads=1']
    elif environment.get('ORANGE_JOB_SOCKET_TEST'):
        command+=['input_cases::socket_cases::','--','--test-threads=1']
    elif environment.get('ORANGE_JOB_HISTORY_TEST'):
        command+=['input_cases::history_cases::','--','--test-threads=1']
    elif environment.get('ORANGE_EXPIRED_ATTEMPT_TEST'):
        command+=['execution_heartbeat_does_not_renew_and_expired_start_cannot_be_retrieved']
    elif environment.get('ORANGE_JOB_REVISION_BROWSER_TEST'):
        command+=['input_cases::cancel_cases::browsers_review_personal_and_organization_job_versions_before_cancelling','--','--exact','--nocapture']
    elif environment.get('ORANGE_QUARANTINE_HEARTBEAT_TEST'):
        command+=['ordinary_authenticated_heartbeat_preserves_quarantine_and_cannot_recreate_execution','--','--exact']
    elif environment.get('ORANGE_DIAGNOSTIC_CORRELATION_TEST'):
        command+=['input_cases::exact_customer_input_requires_start_and_rechecks_deletion_on_real_mtls','--','--exact','--nocapture']
    elif environment.get('ORANGE_AUDIO_GRANT_TESTS'):
        if environment.get('ORANGE_SIX_JOURNEY_TEST'):command+=['transcription_input::one_no_gpu_account_traverses_consumption_supply_team_privacy_purchase_and_refund','--','--exact','--test-threads=1']
        else:command+=['transcription_input::','--','--test-threads=1']
    elif environment.get('ORANGE_TRANSCRIPTION_OBSERVATION_TESTS'):
        command+=['transcription_observation::','--','--test-threads=1']
    elif environment.get('ORANGE_REAL_WORKER_PROFILE'):
        if environment.get('ORANGE_WEB_COMPUTE_TESTS'):command+=['input_cases::cpu_cases::pricing_cases::browser_cases','--','--test-threads=1']
        elif environment.get('ORANGE_CANCEL_REAL_TESTS'):command+=['input_cases::cpu_cases::pricing_cases::cancel_real','--','--test-threads=1']
        elif environment.get('ORANGE_CLEANUP_REAL_TESTS'):command+=['input_cases::cpu_cases::pricing_cases::cleanup_real','--','--test-threads=1']
        elif environment.get('ORANGE_PRICED_WORKER_TESTS'):command+=['input_cases::cpu_cases::pricing_cases','--','--test-threads=1','--skip','input_cases::cpu_cases::pricing_cases::cleanup_real','--skip','input_cases::cpu_cases::pricing_cases::cancel_real']
        elif environment.get('ORANGE_WORKER_HOST_TESTS'):command+=['input_cases::cpu_cases::host_cases','--','--test-threads=1']
        else:command+=['input_cases::cpu_cases','--','--test-threads=1','--skip','input_cases::cpu_cases::host_cases','--skip','input_cases::cpu_cases::pricing_cases']
    elif environment.get('ORANGE_QUEUE_EXPIRY_TESTS'):command+=['input_cases::queue_expiry','--','--test-threads=1']
    elif environment.get('ORANGE_WITHDRAW_TESTS'):command+=['input_cases::withdrawals','--','--test-threads=1']
    elif environment.get('ORANGE_JOB_QUERY_TESTS'):command+=['input_cases::job_queries','--','--test-threads=1']
    elif environment.get('ORANGE_CANCEL_PROTOCOL_TESTS'):command+=['input_cases::cancel_cases','--','--test-threads=1']
    elif environment.get('ORANGE_CLEANUP_PROTOCOL_TESTS'):command+=['input_cases::cleanup_cases','--','--test-threads=1']
    elif environment.get('ORANGE_WORKER_OUTPUT_TESTS'):command+=['input_cases::output_cases','--','--test-threads=1']
    elif environment.get('ORANGE_WORKER_RENEWAL_TESTS'):command+=['input_cases::renewal_cases','--','--test-threads=1']
    passed=False
    with subprocess.Popen(command,cwd=ROOT,env=environment,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True) as process:
        for line in process.stdout:
            sys.stdout.write(line);sys.stdout.flush()
            if re.search(r'test result: ok\. [1-9][0-9]* passed; 0 failed;',line):passed=True
        code=process.wait()
    if code:raise SystemExit(code)
    if not passed:raise SystemExit('No actual selected Worker tests passed; refusing an empty-filter success')
    subprocess.run([str(cargo),'build','-p','orange-control-plane','--bin','orange-worker-gateway','--locked'],cwd=ROOT,env=environment,check=True)
if environment.get('ORANGE_SIX_JOURNEY_TEST'):print('PASS six product journeys on the local CPU marketplace; isolated merchant and enrollment fixtures')
elif environment.get('ORANGE_AUDIO_GRANT_TESTS'):print('PASS bounded ASR input grants, real authenticated range reads and revocation')
elif environment.get('ORANGE_TRANSCRIPTION_OBSERVATION_TESTS'):print('PASS real registered CPU ASR observation on the mTLS gateway; independent review fixture, no Job admission or charge')
elif environment.get('ORANGE_QUEUE_EXPIRY_TESTS'):print('PASS actual PG deadline/queue release and protocol fencing; identity/scheduler/ACK fixtures, no physical cleanup claim')
elif environment.get('ORANGE_WITHDRAW_TESTS'):print('PASS actual HTTP router/PG withdrawal barriers, old-snapshot races, original-requester authority and existing cancellation with separate declared cleanup; identity/quote/cleanup fixtures, no new physical model certification')
elif environment.get('ORANGE_JOB_QUERY_TESTS'):print('PASS actual current-user HTTP router/PostgreSQL metadata isolation and pagination; declared identity/quote fixtures, no network mTLS/model or output-content certification')
elif not environment.get('ORANGE_REAL_WORKER_PROFILE'):print('PASS real mTLS/PostgreSQL control and E08 SQLite/Broker receive/start barriers; explicit CA, profile, OS/device and raw-ACK fixtures, no model engine, physical cleanup or settlement')
