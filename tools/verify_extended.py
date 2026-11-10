"""Independent oracle, actual SIGKILLs and bounded-memory database checks (Linux)."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import selectors
import subprocess
import sys
import time

from verify_database import Client

ROOT=Path(__file__).resolve().parents[1]
KIT=ROOT/'tools/minivectordb-legacy-testkit'
sys.path.insert(0,str(KIT))
from minivectordb_testkit.__main__ import main as kit_main

def write(path,value):
    path.write_text(json.dumps(value,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')

def oracle(binary,out,root):
    reports=[]
    for seed in [20261110,20261111,20261112]:
        directory=out/str(seed)
        directory.mkdir()
        dataset=directory/'dataset.json'
        assert kit_main(['generate','--out',str(dataset),'--rows','96','--dim','8','--queries','24','--k','10','--seed',str(seed)])==0
        assert kit_main(['truth','--dataset',str(dataset),'--out',str(directory/'reference.json')])==0
        source=json.loads(dataset.read_text())
        data=root/str(seed)
        data.mkdir()
        with (directory/'transcript.txt').open('wb') as log:
            c=Client(binary,data,log,evidence=directory)
            try:
                c.sql('CREATE TABLE docs (id BIGINT PRIMARY KEY, category VARCHAR(8), embedding VECTOR(8));')
                values=','.join(f"({r['id']},'{r['category']}','{json.dumps(r['vector'])}')" for r in source['rows'])
                c.sql('INSERT INTO docs VALUES '+values+';')
                c.sql('CREATE VECTOR INDEX vectors ON docs(embedding);')
                for mode in ['exact','ann']:
                    if mode=='ann':
                        c.stop()
                        c=Client(binary,data,log,evidence=directory)
                    candidates={}
                    for q in source['queries']:
                        where='' if q['category'] is None else f"WHERE category='{q['category']}'"
                        sql=f"SELECT id,L2_DISTANCE(embedding,'{json.dumps(q['vector'])}') AS distance FROM docs {where} ORDER BY distance LIMIT {q['k']};"
                        result=c.sql(sql,mode=mode)
                        candidates[q['id']]=[dict(id=r[0],distance=r[1]) for r in result['rows']]
                    actual=directory/f'actual-{mode}.json'
                    write(actual,candidates)
                    report=directory/f'check-{mode}.json'
                    code=kit_main(['check','--dataset',str(dataset),'--results',str(actual),'--mode',mode,'--out',str(report)])
                    reports.append(dict(seed=seed,mode=mode,exit_code=code,report=json.loads(report.read_text())))
                    assert code==0,reports[-1]
                c.stop()
            finally:
                if c.proc.poll() is None:c.stop(kill=True)
        print(f'PASS oracle seed {seed}: exact + ann (actual SQL responses)',flush=True)
    write(out/'result.json',dict(passed=True,query_count=144,reports=reports))

def crash(binary,out,root):
    reports=[]
    for number,delay in enumerate([0,0.0005,0.002,0.008,0.02,0.05]):
        data=root/str(number)
        data.mkdir()
        evidence=out/f'crash-{number}'
        evidence.mkdir()
        with (evidence/'transcript.txt').open('wb') as log:
            c=Client(binary,data,log,evidence=evidence,compress_input=True)
            try:
                c.sql('CREATE TABLE t (id BIGINT PRIMARY KEY, s VARCHAR(512));')
                c.sql("INSERT INTO t VALUES (0,'acknowledged');")
                c.sql('BEGIN;')
                payload='x'*512
                for first in range(1,8001,100):
                    values=','.join(f"({n},'{payload}')" for n in range(first,first+100))
                    c.sql('INSERT INTO t VALUES '+values+';')
                request=c.send_sql('COMMIT;')
                if delay:time.sleep(delay)
                pending=c.stop(kill=True)
                acknowledged=any(json.loads(line)=={'id':request['id'],'ok':True}
                                 for line in pending.splitlines(keepends=True) if line.endswith(b'\n'))
                killed_size=(data/'commits.mv1').stat().st_size
                c=Client(binary,data,log,evidence=evidence,compress_input=True)
                result=c.sql('SELECT id FROM t ORDER BY id;')['rows']
                keys=[r[0] for r in result]
                assert keys in [[0],list(range(8001))],(number,len(keys))
                if acknowledged:assert len(keys)==8001
                # Append after recovery, then recover again: stale tail cannot hide later commits.
                c.sql("INSERT INTO t VALUES (9000,'new acknowledged commit');")
                c.stop(kill=True)
                c=Client(binary,data,log,evidence=evidence,compress_input=True)
                c.sql('SELECT id FROM t WHERE id>=9000;',rows=[[9000]])
                assert len(c.sql('SELECT id FROM t;')['rows'])==len(keys)+1
                c.sql('BEGIN;')
                c.sql('DELETE FROM t;')
                c.stop(kill=True)
                c=Client(binary,data,log,evidence=evidence,compress_input=True)
                assert len(c.sql('SELECT id FROM t;')['rows'])==len(keys)+1
                c.stop()
                reports.append(dict(case=number,commit_delay_seconds=delay,killed_log_bytes=killed_size,commit_ack_observed=acknowledged,visible_rows_after_kill=len(keys),atomic=True,subsequent_commit_survived=True,uncommitted_delete_rolled_back=True))
                print(f'PASS commit SIGKILL delay={delay}: {len(keys)} visible rows; later commit retained',flush=True)
            finally:
                if c.proc.poll() is None:c.stop(kill=True)
    write(out/'result.json',dict(passed=True,cases=reports))

def large(binary,out,root):
    count=24576
    payload='x'*8192
    data=root/'large'
    data.mkdir()
    spec=dict(rows=count,text_bytes=8192,vector_dimension=4,batch_rows=48,transaction_rows=count,sort_bytes=4096)
    write(out/'input-spec.json',spec)
    os.environ['MINIVECTORDB_SORT_BYTES']='4096'
    with (out/'transcript.txt').open('wb') as log:
        c=Client(binary,data,log,evidence=out,compress_input=True)
        try:
            c.sql('CREATE TABLE t (id BIGINT PRIMARY KEY, category VARCHAR(1), content VARCHAR(8192), v VECTOR(4));')
            c.sql('CREATE VECTOR INDEX vi ON t(v);')
            c.sql('BEGIN;')
            for first in range(1,count+1,48):
                values=','.join(f"({n},'{chr(97+n%3)}','{payload}','[{n%17},{n%7},0,0]')" for n in range(first,min(first+48,count+1)))
                result=c.sql('INSERT INTO t VALUES '+values+';')
                assert result['affected_rows']==min(48,count-first+1)
                if first%3072==1:print(f'large transaction staged: {first-1}/{count}',flush=True)
            c.sql("SELECT id FROM t WHERE category='a' ORDER BY L2_DISTANCE(v,'[0,0,0,0]') LIMIT 10;",rows=[[357*n] for n in range(1,11)])
            c.sql('COMMIT;')
            c.stop(kill=True)
            log_bytes=(data/'commits.mv1').stat().st_size
            limit=int(Path('/sys/fs/cgroup/memory.max').read_text())
            assert count*8192>limit,(spec,limit)
            interruptions=[]
            for attempt in range(2):
                stderr=(out/f'recovery-kill-{attempt}.stderr').open('wb')
                proc=subprocess.Popen([binary,'--data-dir',str(data)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=stderr)
                selector=selectors.DefaultSelector()
                selector.register(proc.stdout,selectors.EVENT_READ)
                deadline=time.monotonic()+10
                observed=False
                while time.monotonic()<deadline:
                    if selector.select(0):break
                    # This file is rebuilt only after checksum validation starts recovery.
                    if list((data/'scratch').glob('mv-*.hash')):
                        observed=True
                        break
                    time.sleep(0.001)
                ready_before_kill=bool(selector.select(0))
                proc.kill()
                raw=proc.communicate(timeout=10)[0]
                stderr.close()
                selector.close()
                (out/f'recovery-kill-{attempt}.stdout').write_bytes(raw)
                interruptions.append(dict(attempt=attempt,recovery_index_observed=observed,ready_before_kill=ready_before_kill,exit_code=proc.returncode))
                assert observed and not ready_before_kill and raw==b'',interruptions[-1]
            c=Client(binary,data,log,evidence=out,compress_input=True)
            c.sql("SELECT id FROM t WHERE category='a' ORDER BY L2_DISTANCE(v,'[0,0,0,0]') LIMIT 10;",rows=[[357*n] for n in range(1,11)])
            actual=c.sql('SELECT id FROM t ORDER BY category;')['rows']
            expected=[[n] for n in sorted(range(1,count+1),key=lambda n:(n%3,n))]
            assert actual==expected,(len(actual),len(expected))
            c.sql('BEGIN;')
            result=c.sql("UPDATE t SET content='changed' WHERE id<=100;")
            assert result['affected_rows']==100
            c.sql('DELETE FROM t WHERE id>100;')
            c.stop(kill=True)
            c=Client(binary,data,log,evidence=out,compress_input=True)
            c.sql('SELECT id FROM t WHERE id>=24574 ORDER BY id;',rows=[[24574],[24575],[24576]])
            c.sql('SELECT content FROM t WHERE id=1;',rows=[[payload]])
            c.stop()
            resources={name:Path('/sys/fs/cgroup',name).read_text() for name in ['memory.max','memory.peak','memory.events','memory.stat','io.stat']}
            write(out/'resources.json',resources)
            assert int(resources['memory.peak'])<=limit
            assert 'oom_kill 0' in resources['memory.events']
            write(out/'result.json',dict(passed=True,input=spec,logical_text_bytes=count*8192,log_bytes=log_bytes,memory_limit_bytes=limit,memory_peak_bytes=int(resources['memory.peak']),recovery_interruptions=interruptions,sorted_rows=len(actual),filtered_topk_ids=[357*n for n in range(1,11)]))
            print(f'PASS larger-than-memory: {count*8192} logical bytes, limit {limit}, peak {resources["memory.peak"].strip()}; recovery interrupted twice',flush=True)
        finally:
            if c.proc.poll() is None:c.stop(kill=True)

def wide(binary,out,root):
    count=16
    text_bytes=7*1024*1024
    payload='z'*text_bytes
    data=root/'wide'
    data.mkdir()
    os.environ['MINIVECTORDB_SORT_BYTES']='4096'
    with (out/'transcript.txt').open('wb') as log:
        c=Client(binary,data,log,evidence=out,compress_input=True)
        try:
            c.sql(f'CREATE TABLE wide (id BIGINT PRIMARY KEY, s VARCHAR({text_bytes}));')
            for key in range(1,count+1):
                c.sql(f"INSERT INTO wide VALUES ({key},'{payload}');")
            actual=c.sql('SELECT id,s FROM wide ORDER BY id DESC LIMIT 1;')['rows']
            assert actual==[[count,payload]],len(actual)
            c.stop()
            resources={name:Path('/sys/fs/cgroup',name).read_text() for name in ['memory.max','memory.peak','memory.events']}
            write(out/'resources.json',resources)
            assert int(resources['memory.max'])<count*text_bytes
            assert 'oom_kill 0' in resources['memory.events']
            write(out/'result.json',dict(passed=True,rows=count,row_text_bytes=text_bytes,
                returned_id=count,memory_limit_bytes=int(resources['memory.max']),
                memory_peak_bytes=int(resources['memory.peak'])))
            print(f'PASS wide projection: {count} rows x {text_bytes} bytes, LIMIT 1',flush=True)
        finally:
            if c.proc.poll() is None:c.stop(kill=True)
            write(out/'resources.json',{
                name: Path('/sys/fs/cgroup',name).read_text()
                for name in ['memory.max','memory.peak','memory.events']
            })

if __name__=='__main__':
    p=argparse.ArgumentParser()
    p.add_argument('--scenario',required=True,choices=['oracle','crash','large','wide'])
    p.add_argument('--binary',required=True)
    p.add_argument('--out',required=True)
    p.add_argument('--data-root',required=True)
    a=p.parse_args()
    out=Path(a.out)
    out.mkdir(parents=True,exist_ok=False)
    data=Path(a.data_root)
    data.mkdir(parents=True,exist_ok=False)
    write(out/'fingerprints.json',{str(p.relative_to(ROOT)):hashlib.sha256(p.read_bytes()).hexdigest() for p in [Path(__file__),ROOT/'Cargo.lock',ROOT/'tools/verify_database.py']})
    globals()[a.scenario](a.binary,out,data)
