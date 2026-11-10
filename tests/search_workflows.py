#!/usr/bin/env python3
"""Local-only index search and rule/MCP integration against synthetic storage."""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import tempfile
import threading
from index_workflows import Broker, IndexedDav, ROOT, sqlite_items, wait, report_binary_path
from mcp_workflows import Client
from contextlib import closing


def run(binary):
    checks=[]
    def passed(message):
        checks.append(message);print('PASS '+message,flush=True)
    with tempfile.TemporaryDirectory(prefix='rdrive-search-',dir='/private/tmp') as temporary:
        root=Path(temporary); remote=root/'remote';remote.mkdir()
        for folder in ['docs','private','deep']:(remote/folder).mkdir()
        for path in ['docs/年度报告.txt','docs/report-one.txt','docs/report-two.txt','private/report-secret.txt','deep/a%_literal.txt','deep/note.tmp']:
            (remote/path).write_text(path)
        IndexedDav.root=remote;IndexedDav.online=True;IndexedDav.failed=set();IndexedDav.gates={};IndexedDav.requests_seen=[]
        server=http.server.ThreadingHTTPServer(('127.0.0.1',0),IndexedDav)
        threading.Thread(target=server.serve_forever,daemon=True).start()
        state=root/'broker';state.mkdir();broker=Broker(binary,state,f'http://127.0.0.1:{server.server_port}/dav/','search')
        try:
            broker.configure();broker.cli('filter','config','--include-temporary');broker.start()
            wait(lambda:len(sqlite_items(broker.home,broker.domain))>=9,'complete synthetic index')
            broker.stop();IndexedDav.online=False;before=len(IndexedDav.requests_seen)
            first=broker.cli('index','search','search','report','--page-size','1')
            second=broker.cli('index','search','search','report','--page-size','1','--offset','1')
            assert first['offline'] and first['engine']=='fts5-trigram' and first['more']
            assert first['items'][0]['path']!=second['items'][0]['path']
            assert len(broker.cli('index','search','search','报告')['items'])==1
            assert len(broker.cli('index','search','search','%_')['items'])==1
            assert broker.cli('index','search','search','OR *')['items']==[]
            assert broker.cli('index','search','search','report','--kind','directory')['items']==[]
            passed('offline substring, short Chinese, literal wildcard, type scope and page queries')
            broker.cli('filter','config','--exclude','private/','--exclude-temporary')
            found=broker.cli('index','search','search','report')
            assert len(found['items'])==2 and all('private/'not in i['path']for i in found['items'])
            assert broker.cli('index','search','search','.tmp')['items']==[]
            assert 'private/report-secret.txt'in sqlite_items(broker.home,broker.domain)
            broker.cli('filter','config','--profile','fixture','--clear-exclude','--include-temporary')
            assert len(broker.cli('index','search','search','secret')['items'])==1
            broker.cli('filter','reset','fixture')
            assert broker.cli('index','search','search','secret')['items']==[]
            assert not broker.cli('filter','test','fixture','private/x.txt')['allowed']
            passed('rule changes immediately filter stored search rows without deleting canonical metadata')
            # Read-only MCP exposes search/status but hides every mutation.
            with closing(Client(binary,broker.home,broker.env)) as client:
                client.initialize()
                catalog=client.request('tools/list',{})['tools']
                names={tool['name']for tool in catalog}
                assert 'rdrive_index_search'in names and 'rdrive_service_status'in names
                assert 'rdrive_filter_config'not in names and 'rdrive_service_start'not in names
                response=client.request('tools/call',{'name':'rdrive_index_search','arguments':{'domain':'search','query':'报告'}})
                assert not response['isError'] and len(response['structuredContent']['data']['items'])==1,response
            with closing(Client(binary,broker.home,broker.env,write=True)) as client:
                client.initialize()
                response=client.request('tools/call',{'name':'rdrive_filter_config','arguments':{'include':['*.txt'],'exclude':['private/']}})
                assert not response['isError'],response
                response=client.tool('filter_config',include=[],exclude=[])
                assert response['include']==[] and response['exclude']==[]
                invalid=client.request('tools/call',{'name':'rdrive_filter_config','arguments':{'exclude':['../escape']}},error=-32602)
                assert invalid.get('code')==-32602,invalid
                invalid=client.request('tools/call',{'name':'rdrive_smart_resolve','arguments':{'domain':'search','mutation':'x','strategy':'unsafe'}},error=-32602)
                assert invalid.get('code')==-32602,invalid
            assert len(IndexedDav.requests_seen)==before,'local search unexpectedly contacted remote storage'
            passed('read-only MCP search/resources, mutation gating and typed glob arrays without network traffic')
        finally:
            broker.close();server.shutdown();server.server_close()
    return {'checks':checks,'passed':len(checks),'binary':report_binary_path(binary),'sha256':hashlib.sha256(binary.read_bytes()).hexdigest()}

if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('binary',nargs='?',type=Path,default=ROOT/'target/debug/rdrive');parser.add_argument('--report',type=Path);args=parser.parse_args()
    report=run(args.binary.resolve())
    if args.report:args.report.write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,indent=2))
