"""Analysis-only local probes, not throughput data; production files untouched."""
import os, sys, json, time, tempfile, threading, concurrent.futures, subprocess, hashlib
sys.path.insert(0, os.path.abspath('benchmarks'))
import frankenphp_windows as d

base=d.REPO+'/target/tools/benchmark-20261101'
franken=base+'/frankenphp/frankenphp.exe'
oha=base+'/oha.exe'
d.PHP_DIR=base+'/php-nts'
d.LOG_DIR=base+'/laravel-analysis.logs'
os.makedirs(d.LOG_DIR,exist_ok=True)
output=d.REPO+'/benchmarks/results/laravel_windows_analysis_20261101_probes.json'
assert not os.path.exists(output)
record={'kind':'semantic/lifecycle correctness probes, NOT performance benchmark',
        'frankenphp':subprocess.check_output([franken,'version'],text=True).strip(),
        'identities':[],'logs':[],'source_hashes':{}}
for p in ['www/laravel/composer.lock','www/laravel/vendor/laravel/octane/bin/frankenphp-worker.php',
          'www/laravel/vendor/symfony/http-foundation/Response.php','benchmarks/frankenphp_windows.py']:
    with open(p,'rb') as f: record['source_hashes'][p]=hashlib.file_digest(f,'sha256').hexdigest()

def remember(server):
    record['identities'].extend({'pid':p,'created':d.created(p)} for p in d.tree([server.pid]))

def wait_file(path,seconds=15):
    deadline=time.monotonic()+seconds
    while not os.path.exists(path):
        if time.monotonic()>deadline: raise TimeoutError(path)
        time.sleep(.01)

def config(directory,script,workers,root,ini,laravel=False):
    env=''
    if laravel:
        settings=dict(d.LARAVEL_ENV,APP_BASE_PATH=root,APP_PUBLIC_PATH=root+'/public',LARAVEL_OCTANE='1')
        env=''.join(f'\t\t\tenv {k} {json.dumps(v)}\n' for k,v in settings.items())
    text=d.caddyfile(f'\tfrankenphp {{\n\t\tnum_threads {workers+1}\n\t\tworker {{\n'
                     + f'\t\t\tfile "{script}"\n\t\t\tnum {workers}\n'+env+'\t\t}\n\t}\n',
                     f'\troot * "{root}"\n\tphp_server {{\n\t\tindex '+os.path.basename(script)+'\n\t}\n')
    if laravel:
        text=d.caddyfile(f'\tfrankenphp {{\n\t\tnum_threads {workers+1}\n\t\tworker {{\n'
                        + f'\t\t\tfile "{script}"\n\t\t\tnum {workers}\n'+env+'\t\t}\n\t}\n',
                        f'\troot * "{root}/public"\n\tphp_server {{\n'
                        '\t\tindex frankenphp-worker.php\n\t\ttry_files {path} frankenphp-worker.php\n\t}\n')
    text=text.replace('http://127.0.0.1:8080, http://localhost:8080','http://127.0.0.1:18080')
    return d.write(directory+'/Caddyfile',text)

try:
 with tempfile.TemporaryDirectory(prefix='hamster-laravel-analysis-') as temporary:
    directory=temporary.replace('\\','/')
    # Exact Symfony Response::send(), with NO artificial Content-Length.
    sent=directory+'/after-send.json'; release=directory+'/release'; done=directory+'/done'
    script='''<?php
require %s;
$handler = static function () {
    $response = new Symfony\\Component\\HttpFoundation\\Response('semantic probe', 200);
    $response->send();
    file_put_contents(%s,json_encode(['pid'=>getmypid(),'sapi'=>PHP_SAPI,
        'fastcgi_finish_request'=>function_exists('fastcgi_finish_request'),
        'frankenphp_finish_request'=>function_exists('frankenphp_finish_request'),
        'headers'=>headers_list()]));
    $deadline=microtime(true)+5;
    while (!is_file(%s) && microtime(true)<$deadline) { usleep(1000); clearstatcache(); }
    file_put_contents(%s,json_encode(['parent_released'=>is_file(%s)]));
};
while(frankenphp_handle_request($handler)) {}
''' % tuple(json.dumps(p) for p in [d.REPO+'/www/laravel/vendor/autoload.php',sent,release,done,release])
    fixture=d.write(directory+'/index.php',script)
    ini=d.write(directory+'/php.ini',d.php_ini(base+'/frankenphp/ext',False))
    environment=dict(os.environ,PHPRC=ini,PHP_INI_SCAN_DIR='')
    caddy=config(directory,fixture,1,directory,ini)
    d.wait_for_free(18080)
    server=d.start([franken,'run','--config',caddy],environment)
    headers=threading.Event(); header_details={}
    def request():
        client=d.http.client.HTTPConnection('127.0.0.1',18080,timeout=15)
        try:
            client.request('GET','/')
            response=client.getresponse()
            header_details.update(status=response.status,headers=response.getheaders())
            headers.set()
            return {'status':response.status,'body':response.read().decode()}
        finally: client.close()
    try:
        d.wait_for_port(18080,process=server); remember(server)
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
            future=executor.submit(request)
            try:
                wait_file(sent)
                with open(sent) as f: details=json.load(f)
                try:
                    received=future.result(timeout=1)
                    complete_before_release=True
                except concurrent.futures.TimeoutError:
                    received=None; complete_before_release=False
                headers_before_release=headers.is_set()
            finally:
                d.write(release,'release')
            received=future.result(timeout=10)
            wait_file(done)
            with open(done) as f: final=json.load(f)
        assert received=={'status':200,'body':'semantic probe'} and final['parent_released']
        record['response_boundary']={'runtime':details,'headers_before_release':headers_before_release,
            'full_body_before_release':complete_before_release,'client':received,
            'headers':header_details,'tail_completed_after_release':True,
            'probe_does_not_infer_full_octane_cleanup_cost':True}
    finally:
        remember(server); d.stop(server); record['logs'].append(server.benchmark_log)
    print('BOUNDARY',json.dumps(record.get('response_boundary')),flush=True)

    # Observe actual Octane PHP-script loop limit, not engine max_requests.
    ini=d.write(directory+'/framework.ini',d.php_ini(d.PHP_DIR+'/ext',True))
    app=d.prepare_laravel(d.REPO+'/www/laravel',directory+'/laravel',ini)
    events=directory+'/worker-events.jsonl'
    prefix='''<?php
$probeToken=bin2hex(random_bytes(8));
$probeEvents=%s;
file_put_contents($probeEvents,json_encode(['phase'=>'start','pid'=>getmypid(),
    'token'=>$probeToken,'limit'=>$_ENV['MAX_REQUESTS'] ?? $_SERVER['MAX_REQUESTS'] ?? 1000])."\\n",FILE_APPEND|LOCK_EX);
?>
''' % json.dumps(events)
    with open(app+'/vendor/laravel/octane/src/Commands/stubs/frankenphp-worker.php') as f: stub=f.read()
    suffix='''
file_put_contents($probeEvents,json_encode(['phase'=>'end','pid'=>getmypid(),
    'token'=>$probeToken,'handled'=>$requestCount ?? null,'limit'=>$maxRequests ?? null])."\\n",FILE_APPEND|LOCK_EX);
'''
    fixture=d.write(app+'/public/frankenphp-worker.php',prefix+stub+suffix)
    f_ini=d.write(directory+'/franken-framework.ini',d.php_ini(base+'/frankenphp/ext',True))
    caddy=config(directory,fixture,4,app,f_ini,True)
    environment=dict(os.environ,PHPRC=f_ini,PHP_INI_SCAN_DIR='')
    environment.pop('MAX_REQUESTS',None)
    d.wait_for_free(18080)
    server=d.start([franken,'run','--config',caddy],environment)
    try:
        d.wait_for_port(18080,process=server); remember(server)
        data=json.loads(subprocess.check_output([oha,'--no-tui','--output-format','json',
             '-n','4200','-c','4','http://127.0.0.1:18080/login'],text=True,
             creationflags=d.CREATE_NO_WINDOW))
        assert data['statusCodeDistribution']=={'200':4200} and not data.get('errorDistribution')
        with open(events) as f: entries=[json.loads(s) for s in f]
        starts=[e for e in entries if e['phase']=='start']; ends=[e for e in entries if e['phase']=='end']
        assert len(starts)>4 and ends and all(e['handled']==1000 for e in ends),entries
        record['octane_default_recycle']={'requests':4200,'statuses':data['statusCodeDistribution'],
            'errors':data.get('errorDistribution',{}),'starts':len(starts),'ends':len(ends),
            'events':entries,'MAX_REQUESTS_unset_in_child_environment':True,
            'not_a_throughput_result':True}
    finally:
        remember(server); d.stop(server); record['logs'].append(server.benchmark_log)
    print('RECYCLE',json.dumps({k:v for k,v in record['octane_default_recycle'].items() if k!='events'}),flush=True)
finally:
    with open(output,'x',encoding='utf-8') as f: json.dump(record,f,indent=2)
