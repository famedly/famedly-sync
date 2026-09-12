#!/usr/bin/env python3
"""Bootstrap a unique disposable stack; execute real Rust tests; always clean up.

Requires Docker Compose, cargo, Python cryptography. No production credentials.
Only sanitized JSON evidence survives; no container logs, mail bodies or keys.
"""
import base64
import json
import os
import re
import html
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding, rsa

ROOT = Path(__file__).resolve().parents[2]


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def request(base, path, data=None, token=None, org=None):
    headers = {'Content-Type': 'application/json'}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    if org:
        headers['x-zitadel-orgid'] = org
    req = urllib.request.Request(base + path, headers=headers,
                                 data=None if data is None else json.dumps(data).encode())
    with urllib.request.urlopen(req, timeout=10) as response:
        return json.load(response)


def authenticate(base, key_path):
    key = json.loads(key_path.read_text())
    def b64(data):
        return base64.urlsafe_b64encode(data).rstrip(b'=').decode()
    now = int(time.time())
    unsigned = '.'.join(b64(json.dumps(part).encode()) for part in [
        {'alg': 'RS256', 'kid': key['keyId']},
        {'iss': key['userId'], 'sub': key['userId'], 'aud': base, 'iat': now, 'exp': now + 300}])
    private = serialization.load_pem_private_key(key['key'].encode(), password=None)
    assert isinstance(private, rsa.RSAPrivateKey)
    assertion = unsigned + '.' + b64(private.sign(unsigned.encode(), padding.PKCS1v15(), hashes.SHA256()))
    req = urllib.request.Request(base + '/oauth/v2/token', data=urllib.parse.urlencode({
        'grant_type': 'urn:ietf:params:oauth:grant-type:jwt-bearer',
        'scope': 'openid urn:zitadel:iam:org:project:id:zitadel:aud', 'assertion': assertion}).encode())
    with urllib.request.urlopen(req, timeout=10) as response:
        return json.load(response)['access_token']


def main():
    os.umask(0o077)
    project = 'sync-onboarding-' + uuid.uuid4().hex[:12]
    work = Path(tempfile.mkdtemp(prefix=project + '-'))
    evidence = Path(os.environ.get('ONBOARDING_ARTIFACTS', ROOT / 'target/onboarding-evidence')).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    port, mail_port = free_port(), free_port()
    while mail_port == port:
        mail_port = free_port()
    base, mailpit = f'http://localhost:{port}', f'http://localhost:{mail_port}'
    env = dict(os.environ, ONBOARDING_WORK=str(work), ONBOARDING_PORT=str(port), ONBOARDING_MAIL_PORT=str(mail_port))
    compose = ['docker', 'compose', '-p', project, '-f', str(Path(__file__).with_name('docker-compose.yaml'))]
    config = {'Port': 8080, 'ExternalPort': port, 'ExternalDomain': 'localhost', 'ExternalSecure': False,
              'TLS': {'Enabled': False}, 'Database': {'postgres': {'Host': 'db', 'Port': 5432, 'Database': 'zitadel',
              'User': {'Username': 'zitadel', 'Password': 'disposable', 'SSL': {'Mode': 'disable'}},
              'Admin': {'Username': 'postgres', 'Password': 'disposable-onboarding', 'SSL': {'Mode': 'disable'}}}},
              'DefaultInstance': {'SMTPConfiguration': {'SMTP': {'Host': 'mailpit:1025'},
              'From': 'no-reply@example.test', 'FromName': 'Onboarding regression'}}}
    (work / 'zitadel.json').write_text(json.dumps(config))
    (work / 'steps.json').write_text(json.dumps({'FirstInstance': {'MachineKeyPath': '/fixture/service-user.json',
        'Org': {'Machine': {'Machine': {'Username': 'onboarding-admin', 'Name': 'Disposable onboarding admin'}, 'MachineKey': {'Type': 1}}}}}))
    summary = {'compose_project': project, 'zitadel_version': '4.15.2', 'zitadel_url': base, 'mailpit_url': mailpit, 'passed': False}
    def interrupted(_signum, _frame):
        raise RuntimeError('Interrupted')
    signal.signal(signal.SIGTERM, interrupted)
    try:
        subprocess.run(compose + ['up', '-d'], env=env, check=True, timeout=240)
        deadline = time.monotonic() + 180
        while True:
            try:
                with urllib.request.urlopen(base + '/debug/ready', timeout=5) as response:
                    assert response.status == 200
                token = authenticate(base, work / 'service-user.json')
                org = request(base, '/management/v1/orgs/me', token=token)['org']['id']
                break
            except (OSError, ValueError, AssertionError):
                if time.monotonic() >= deadline:
                    raise RuntimeError('Zitadel bootstrap/auth readiness timeout') from None
                time.sleep(1)
        project_id = request(base, '/management/v1/projects', {'name': project, 'projectRoleAssertion': False,
            'projectRoleCheck': False, 'hasProjectCheck': False, 'privateLabelingSetting': 'PRIVATE_LABELING_SETTING_UNSPECIFIED'}, token, org)['id']
        request(base, f'/management/v1/projects/{project_id}/roles', {'roleKey': 'User', 'displayName': 'User', 'group': ''}, token, org)
        readback = request(base, f'/management/v1/projects/{project_id}', token=token, org=org)
        roles = request(base, f'/management/v1/projects/{project_id}/roles/_search', {}, token, org)
        assert any(role['key'] == 'User' for role in roles['result'])
        idp = request(base, '/management/v1/idps/ldap', {'name': project + '-idp',
            'servers': ['ldap://unused.invalid:389'], 'startTls': False, 'baseDn': 'dc=example,dc=test',
            'bindDn': 'cn=admin,dc=example,dc=test', 'bindPassword': 'disposable',
            'userBase': 'dn', 'userObjectClasses': ['inetOrgPerson'], 'userFilters': ['(objectClass=inetOrgPerson)'],
            'attributes': {'idAttribute': 'uid'}, 'providerOptions': {'isCreationAllowed': True}}, token, org)['id']
        app = {'url': base, 'key_file': str(work / 'service-user.json'), 'organization_id': org, 'project_id': project_id, 'idp_id': idp}
        (work / 'sync.json').write_text(json.dumps(app))
        summary['project'] = readback
        summary['roles'] = roles
        env.update(ONBOARDING_CONFIG=str(work / 'sync.json'), ONBOARDING_MAILPIT=mailpit,
                   ONBOARDING_EVIDENCE=str(evidence / 'onboarding.json'), RUST_TEST_THREADS='1')
        subprocess.run(['cargo', 'test', '--locked', '--test', 'onboarding-state'], cwd=ROOT, env=env, check=True, timeout=600)
        subprocess.run(['cargo', 'test', '--locked', '--test', 'onboarding-state', '--', '--ignored'], cwd=ROOT, env=env, check=True, timeout=600)
        subprocess.run(['cargo', 'test', '--locked', '--test', 'onboarding', '--', '--ignored'], cwd=ROOT, env=env, check=True, timeout=600)
        # Inspect only the selected fixture invitation in memory; never persist code URLs/bodies.
        report = json.loads((evidence / 'onboarding.json').read_text())
        selected = next(case for case in report['cases'] if case['verify_email'])
        message_id = selected['messages'][0]['id']
        message = request(mailpit, f'/api/v1/message/{message_id}')
        links = [html.unescape(link) for link in re.findall(r'href=["\']([^"\']+)', message['HTML'])]
        link = next(link for link in links if '/invite' in link and 'code=' in link)
        parsed = urllib.parse.urlparse(link)
        assert parsed.hostname == 'localhost' and parsed.port == port
        query = urllib.parse.parse_qs(parsed.query)
        assert query['userID'] == [selected['user_id']]
        try:
            with urllib.request.urlopen(link, timeout=10) as response:
                page = response.read().decode()
        except Exception:
            raise RuntimeError('Invitation landing page unavailable') from None
        assert 'name="password"' in page and 'name="passwordconfirm"' in page
        request(base, f"/v2/users/{selected['user_id']}/invite_code/verify",
                {'verificationCode': query['code'][0]}, token, org)
        verified = request(base, f"/v2/users/{selected['user_id']}", token=token, org=org)
        assert verified['user']['human']['email']['isVerified'] is True
        summary['first_auth'] = {'invitation_password_form': True, 'invite_code_verified': True,
                                 'email_verified_after_invite': True, 'password_set_by_sync': False}
        env.update(VERIFY_EMAIL_CONFIG=str(work / 'sync.json'), VERIFY_EMAIL_MAILPIT=mailpit,
                   VERIFY_EMAIL_EVIDENCE=str(evidence / 'verify-email.json'))
        subprocess.run(['cargo', 'test', '--locked', '--test', 'verify-email', '--', '--ignored'], cwd=ROOT, env=env, check=True, timeout=600)
        summary['passed'] = True
    finally:
        result = subprocess.run(compose + ['down', '-v', '--remove-orphans'], env=env, timeout=90, check=False)
        summary['cleanup_exit_code'] = result.returncode
        remaining = subprocess.check_output(['docker', 'ps', '-aq', '--filter', f'label=com.docker.compose.project={project}'], text=True).strip()
        summary['cleanup_verified'] = not remaining
        shutil.rmtree(work)
        (evidence / 'bootstrap.json').write_text(json.dumps(summary, indent=2))
        if result.returncode or remaining:
            raise RuntimeError('Disposable stack cleanup failed')
    print(f'Onboarding integration passed; safe evidence: {evidence}')


if __name__ == '__main__':
    main()
