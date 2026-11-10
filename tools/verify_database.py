"""Real process boundary checks; expected values are hand derived, never DB output."""
import argparse
import gzip
import json
from pathlib import Path
import subprocess
import tempfile

class Client:
    def __init__(self, binary, data, transcript, evidence=None, compress_input=False):
        self.transcript = transcript
        evidence = data if evidence is None else evidence
        self.err = open(evidence / 'stderr.log', 'ab')
        self.rawout = open(evidence / 'stdout.jsonl', 'ab')
        self.rawin = (gzip.open(evidence / 'requests.jsonl.gz', 'ab', compresslevel=1)
                      if compress_input else open(evidence / 'requests.jsonl', 'ab'))
        self.compress_input = compress_input
        self.proc = subprocess.Popen([binary, '--data-dir', str(data)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.err)
        assert self.read() == {'type': 'ready'}
        self.seq = 0
    def read(self):
        line = self.proc.stdout.readline()
        self.rawout.write(line)
        self.rawout.flush()
        self.transcript.write(line)
        self.transcript.flush()
        if not line:
            code = self.proc.wait(timeout=10)
            self.transcript.write(f'EXIT {code}\n'.encode())
            raise AssertionError(f'database exited before response (exit {code})')
        return json.loads(line)
    def send_sql(self, sql, ident=None, mode='exact'):
        self.seq += 1
        request = dict(id=self.seq if ident is None else ident, op='execute', sql=sql, search_mode=mode)
        if self.compress_input and len(sql) > 1024:
            self.transcript.write(f'REQUEST {self.seq} ({len(sql)} SQL characters, see raw input)\n'.encode())
        else:
            self.transcript.write(('REQUEST ' + json.dumps(request, ensure_ascii=False) + '\n').encode())
        data=(json.dumps(request) + '\n').encode()
        self.rawin.write(data)
        self.rawin.flush()
        self.proc.stdin.write(data)
        self.proc.stdin.flush()
        return request
    def sql(self, sql, ok=True, rows=None, ident=None, mode='exact'):
        request=self.send_sql(sql, ident, mode)
        response = self.read()
        assert response['id'] == request['id'] and type(response['id']) is type(request['id']), response
        assert response.get('ok') is ok, (sql, response)
        if not ok:
            assert isinstance(response.get('error', {}).get('code'), str) and response['error']['code'], response
        if rows is not None:
            assert response.get('rows') == rows, (sql, response, rows)
        return response
    def stop(self, kill=False):
        pending = b''
        if kill:
            self.proc.kill()
        else:
            data=b'{"id":"shutdown","op":"shutdown"}\n'
            self.rawin.write(data)
            self.rawin.flush()
            self.proc.stdin.write(data)
            self.proc.stdin.flush()
            assert self.read() == {'id': 'shutdown', 'ok': True}
            self.proc.stdin.close()
        self.proc.wait(timeout=10)
        if kill:
            pending=self.proc.stdout.read()
            self.rawout.write(pending)
            self.transcript.write(pending)
        self.transcript.write(('EXIT ' + str(self.proc.returncode) + '\n').encode())
        if not kill:
            assert self.proc.returncode == 0
            assert self.proc.stdout.read() == b''
        self.err.close()
        self.rawout.close()
        self.rawin.close()
        self.proc.stdout.close()
        self.proc.stdin.close()
        return pending

def run(binary, out):
    out.mkdir(parents=True, exist_ok=False)
    data = out / 'data'
    data.mkdir()
    with (out/'transcript.jsonl').open('wb') as transcript:
        c = Client(binary, data, transcript)
        try:
            c.sql('CREATE TABLE docs (id BIGINT PRIMARY KEY, category VARCHAR(3), value BIGINT, embedding VECTOR(2));')
            c.sql("INSERT INTO docs VALUES (9007199254740993,'甲😀乙',-9223372036854775808,'[16777217,0]'), (2,'甲',9223372036854775807,'[16777216,0]');")
            competing = subprocess.run([binary, '--data-dir', str(data)], input=b'',
                                       capture_output=True, timeout=10, check=False)
            (out / 'competing-stdout.log').write_bytes(competing.stdout)
            (out / 'competing-stderr.log').write_bytes(competing.stderr)
            (out / 'competing-exit.txt').write_text(str(competing.returncode), encoding='utf-8')
            assert competing.returncode != 0 and not competing.stdout, competing
            c.sql('SELECT id,value FROM docs ORDER BY id;', rows=[[2,9223372036854775807],[9007199254740993,-9223372036854775808]], ident=9007199254740993)
            c.sql('SELECT id FROM docs WHERE id=2;', rows=[[2]],
                  ident={'$serde_json::private::Number': '1'})
            c.sql("SELECT id,L2_DISTANCE(embedding,'[16777216,0]') AS distance FROM docs ORDER BY distance LIMIT 5;", rows=[[2,0.0],[9007199254740993,0.0]], ident='字符串ID')
            for value in ["9223372036854775808", "-9223372036854775809", "1.0", "1e0"]:
                c.sql(f"INSERT INTO docs VALUES ({value},'x',1,'[0,0]');", ok=False)
            for vec in ['[NaN,0]', '[Infinity,0]', '[1e40,0]', '[0]', '[true,0]']:
                c.sql(f"INSERT INTO docs VALUES (3,'x',1,'{vec}');", ok=False)
            c.sql("INSERT INTO docs VALUES (3,'甲😀乙丙',1,'[0,0]');", ok=False)
            c.sql('BEGIN;')
            c.sql("UPDATE docs SET category='前' WHERE id=2;")
            c.sql("INSERT INTO docs VALUES (3,'x',1,'[0,0]'),(2,'x',1,'[0,0]');", ok=False)
            c.sql("UPDATE docs SET embedding='[0]' WHERE id=2;", ok=False)
            c.sql("SELECT id,category FROM docs ORDER BY id;", rows=[[2,'前'],[9007199254740993,'甲😀乙']])
            c.sql('COMMIT;')
            c.stop(kill=True)
            c = Client(binary, data, transcript)
            c.sql('SELECT id,category FROM docs ORDER BY id;', rows=[[2,'前'],[9007199254740993,'甲😀乙']])
            c.sql('BEGIN;')
            c.sql('DELETE FROM docs;')
            c.stop(kill=True)
            c = Client(binary, data, transcript)
            c.sql('SELECT id FROM docs ORDER BY id;', rows=[[2],[9007199254740993]])
            c.sql('CREATE TABLE empty (id BIGINT PRIMARY KEY, value BIGINT, embedding VECTOR(2));')
            for sql in ["SELECT missing FROM empty;", "SELECT id FROM empty WHERE missing=1;", "SELECT id FROM empty ORDER BY missing LIMIT 0;", "SELECT L2_DISTANCE(embedding,'[0]') AS d FROM empty;", "UPDATE empty SET value='x';", "UPDATE empty SET id=1;", 'SELECT id FROM empty LIMIT -1;', 'SELECT id FROM empty; DELETE FROM docs;']:
                c.sql(sql, ok=False)
            c.sql("CREATE TABLE text (id BIGINT PRIMARY KEY, s VARCHAR(16));")
            c.sql("INSERT INTO text VALUES (3,'b'),(2,'a;''中'),(1,'b');")
            c.sql('SELECT * FROM text ORDER BY s DESC;', rows=[[1,'b'],[3,'b'],[2,"a;'中"]])
            c.sql('BEGIN;')
            c.sql('CREATE VECTOR INDEX vi ON docs(embedding);')
            c.sql('ROLLBACK;')
            c.sql('CREATE VECTOR INDEX vi ON docs(embedding);')
            c.sql('CREATE VECTOR INDEX vi ON docs(embedding);', ok=False)
            c.stop()
        finally:
            if c.proc.poll() is None:
                c.stop(kill=True)
    print('PASS: protocol, types, failed-statement atomicity, kill durability, uncommitted rollback, empty-table validation, index DDL')

if __name__ == '__main__':
    p=argparse.ArgumentParser()
    p.add_argument('--binary', required=True)
    p.add_argument('--out', required=True)
    args=p.parse_args()
    run(args.binary, Path(args.out))
