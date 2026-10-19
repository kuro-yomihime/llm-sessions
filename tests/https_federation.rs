#![cfg(all(feature = "serve", feature = "http"))]
//! Verify the real client through TLS termination and a proxy path prefix.
use std::process::Command;

#[test]
fn https_proxy_preserves_federation_identity_and_verifies_certificate_trust() {
    let root = tempfile::tempdir().unwrap();
    let script = r"
import os, pathlib, subprocess, sys, json, ssl, threading, http.server, urllib.request, time, re, signal
root, binary = pathlib.Path(sys.argv[1]), sys.argv[2]
token = 'fixture-proxy-bearer-123456'
origin = '00000000-0000-4000-8000-000000000002'
for name in ['peer', 'observer']:
    (root/name).mkdir()
def env(name):
    home = root/name
    e = dict(os.environ, HOME=str(home), XDG_CONFIG_HOME=str(home/'config'), XDG_STATE_HOME=str(home/'state'), XDG_DATA_HOME=str(home/'data'), XDG_CACHE_HOME=str(home/'cache'), PROXY_TOKEN=token)
    for key in ['LLMS_TOKEN', 'LLMS_BIND', 'SSL_CERT_FILE', 'SSL_CERT_DIR']: e.pop(key, None)
    return e
state = root/'peer/state/llm-sessions'; state.mkdir(parents=True)
(state/'origin-id').write_text(origin)
store = root/'peer/.omp/agent/sessions/work'; store.mkdir(parents=True)
(store/'tls-session.jsonl').write_text(json.dumps({'type':'session','version':3,'id':'tls-session','timestamp':'2026-01-01T00:00:00Z','cwd':'/workspace','title':'TLS proxy transcript'})+'\n')
def openssl(*args):
    subprocess.run(['openssl',*args],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
ca, ca_key, cert, key, csr = [str(root/n) for n in ['ca.pem','ca.key','server.pem','server.key','server.csr']]
openssl('req','-x509','-newkey','rsa:2048','-nodes','-keyout',ca_key,'-out',ca,'-days','1','-subj','/CN=Fixture CA','-addext','basicConstraints=critical,CA:TRUE')
openssl('req','-newkey','rsa:2048','-nodes','-keyout',key,'-out',csr,'-subj','/CN=localhost')
ext = root/'extensions'; ext.write_text('subjectAltName=IP:127.0.0.1,DNS:localhost\nbasicConstraints=critical,CA:FALSE\n')
openssl('x509','-req','-in',csr,'-CA',ca,'-CAkey',ca_key,'-CAcreateserial','-out',cert,'-days','1','-extfile',str(ext))
log = root/'server.log'
with log.open('w') as output:
    backend = subprocess.Popen([binary,'--read-only','serve','--bind','127.0.0.1:0','--token',token],env=env('peer'),stdout=output,stderr=output)
proxy = None
try:
    deadline = time.monotonic()+15
    while time.monotonic() < deadline:
        # Ignore an incomplete readiness line, including a partial port.
        complete = log.read_text().rpartition('\n')[0]
        match = re.search(r'on http://(127\.0\.0\.1:\d+)',complete)
        if match: break
        assert backend.poll() is None, log.read_text()
        time.sleep(.025)
    assert match, log.read_text()
    upstream = 'http://'+match.group(1)
    seen = []
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self,*args): pass
        def do_GET(self):
            seen.append(self.path)
            assert self.path.startswith('/proxy/'), self.path
            request = urllib.request.Request(upstream+self.path[len('/proxy'):],headers={'Authorization':self.headers.get('Authorization','')})
            with urllib.request.urlopen(request,timeout=15) as response:
                body=response.read()
                self.send_response(response.status); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
    proxy = http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); tls.load_cert_chain(cert,key)
    proxy.socket=tls.wrap_socket(proxy.socket,server_side=True)
    threading.Thread(target=proxy.serve_forever,daemon=True).start()
    url='https://127.0.0.1:'+str(proxy.server_port)+'/proxy'
    config = root/'observer/config/llm-sessions'; config.mkdir(parents=True)
    (config/'hosts.json').write_text(json.dumps({'hosts':[{'name':'tls-peer','url':url,'origin_id':origin,'token_env':'PROXY_TOKEN','sources':['omp']}]}))
    subprocess.run([binary,'origin','--init'],env=env('observer'),check=True,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    def cli(trusted,*args):
        e=env('observer')
        if trusted: e['SSL_CERT_FILE']=ca
        return subprocess.run([binary,'--read-only',*args],env=e,capture_output=True,text=True,timeout=30)
    untrusted=cli(False,'show','tls-session','--origin',origin,'--source','omp','--json')
    assert untrusted.returncode != 0, 'untrusted TLS unexpectedly accepted'
    listed=cli(True,'list','--json','--limit','0')
    assert listed.returncode == 0, listed.stderr
    rows=json.loads(listed.stdout)
    assert len(rows)==1 and rows[0]['origin_id']==origin and rows[0]['routes'][0]['hops'][0]['url']==url, (rows, listed.stderr)
    opened=cli(True,'show','tls-session','--origin',origin,'--source','omp','--json')
    assert opened.returncode == 0, opened.stderr
    assert json.loads(opened.stdout)['title']=='TLS proxy transcript'
    assert any(p.startswith('/proxy/v1/federation/capabilities') for p in seen),seen
    assert any(p.startswith('/proxy/v1/federation/list?') for p in seen),seen
    assert any(p.startswith('/proxy/v1/sessions/tls-session/transcript?') for p in seen),seen
    assert token not in listed.stdout+listed.stderr+opened.stdout+opened.stderr+untrusted.stderr
    print('TLS proxy: untrusted CA rejected; trusted platform CA accepted; prefixed discovery and qualified transcript preserve origin')
finally:
    if proxy: proxy.shutdown(); proxy.server_close()
    backend.send_signal(signal.SIGTERM)
    try: backend.wait(timeout=5)
    except subprocess.TimeoutExpired: backend.kill(); backend.wait()
";
    let output = Command::new("python3")
        .args(["-c", script])
        .arg(root.path())
        .arg(env!("CARGO_BIN_EXE_llm-sessions"))
        .output()
        .expect("HTTPS fixture requires python3 and openssl");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
}
