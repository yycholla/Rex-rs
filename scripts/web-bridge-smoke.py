#!/usr/bin/env python3
"""Exercises only a disposable Rex server; never attaches to a user's server."""
import json, os, pathlib, select, subprocess, sys, tempfile, time

binary = str(pathlib.Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='rex-web-bridge-smoke-') as directory:
    root = pathlib.Path(directory)
    (root / 'init.lua').write_text('-- isolated web bridge test\n')
    endpoint = 'unix://' + str(root / 'server.sock')
    log = open(root / 'server.log', 'w+')
    env = dict(os.environ, SHELL='/bin/sh', HOME=str(root), XDG_CONFIG_HOME=str(root/'config'), XDG_DATA_HOME=str(root/'data'), XDG_CACHE_HOME=str(root/'cache'))
    server = subprocess.Popen(['rex','server','run','--listen',endpoint,'--state-dir',str(root/'state'),'--config',str(root/'init.lua'),'--allow-anonymous'],env=env,stdin=subprocess.DEVNULL,stdout=log,stderr=log)
    bridge = None
    serial = 0
    def start():
        return subprocess.Popen([binary,'--endpoint',endpoint],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True)
    def call(op, **kw):
        global serial
        serial += 1
        bridge.stdin.write(json.dumps(dict(id=str(serial),op=op,**kw))+'\n'); bridge.stdin.flush()
        assert select.select([bridge.stdout],[],[],10)[0], 'bridge response timeout'
        line = bridge.stdout.readline()
        assert line, 'bridge stopped'
        result = json.loads(line)
        assert result['id'] == str(serial), result
        return result
    def ok(op, **kw):
        result = call(op,**kw)
        assert result['ok'], result
        return result['result']
    def wait(predicate):
        deadline = time.monotonic()+10
        while time.monotonic()<deadline:
            s = ok('snapshot')
            if predicate(s): return s
            time.sleep(.05)
        raise AssertionError('snapshot condition timed out: '+json.dumps({k: [''.join(r['text'] for r in row) for row in f['lines']] for k,f in s['frames'].items()}))
    def command(action, **kw):
        s=ok('snapshot')
        return ok('command',generation=s['generation'],confirmed=True,action=action,**kw)
    try:
        bridge = start()
        s = wait(lambda s:s['connected'])
        assert not s['sessions'], 'attaching must not create a session'
        stale=call('command',generation='stale',action='new-session',confirmed=True)
        assert not stale['ok']
        denied=call('command',generation=s['generation'],action='new-session')
        assert not denied['ok']
        assert command('new-session',title='Bridge isolated fixture')['ack']=='applied'
        s=wait(lambda s:bool(s['blocks']) and bool(s['frames']))
        sid=s['sessions'][0]['id']; bid=s['blocks'][0]['id']
        # A live terminal stream can precede the interactive shell's startup/mode negotiation.
        wait(lambda s:any('❯' in ''.join(run['text'] for run in row) or '$ ' in ''.join(run['text'] for run in row) for row in s['frames'][bid]['lines']))
        time.sleep(.2)
        assert ok('input',generation=s['generation'],blockId=bid,paste=True,text="printf '\\033[31mBRIDGE_%s\\033[0m\\n' NATIVE_OK")['ack']=='queued'
        assert ok('input',generation=s['generation'],blockId=bid,key='enter')['ack']=='queued'
        def sentinel(s):
            f=s['frames'].get(bid)
            return f and any('BRIDGE_NATIVE_OK' in ''.join(run['text'] for run in row) for row in f['lines'])
        s=wait(sentinel)
        colored=[run for row in s['frames'][bid]['lines'] for run in row if 'BRIDGE_NATIVE_OK' in run['text']]
        assert colored and colored[0]['foreground'] != s['frames'][bid]['foreground']
        for char in "printf SINGLE_%s INPUT_OK":
            assert ok('input',generation=s['generation'],blockId=bid,text=char)['ack']=='queued'
        assert ok('input',generation=s['generation'],blockId=bid,key='enter')['ack']=='queued'
        wait(lambda s:any('SINGLE_INPUT_OK' in ''.join(run['text'] for run in row) for row in s['frames'][bid]['lines']))
        assert command('split-right',blockId=bid)['ack']=='applied'
        s=wait(lambda s:len(s['blocks'])>=2)
        assert command('new-window',sessionId=sid,title='Second tab')['ack']=='applied'
        s=wait(lambda s:len(s['sessions'][0]['windows'])==2)
        generation=s['generation']
        original_owners={b['id']:b['owner'] for b in s['blocks']}
        bridge.stdin.close(); bridge.wait(timeout=5)
        bridge=start()
        s=wait(lambda s:s['connected'] and any(b['id']==bid for b in s['blocks']))
        assert s['generation'] != generation
        assert all(b['owner'] in (None,original_owners[b['id']]) for b in s['blocks']), 'attach changed size ownership'
        assert not any(b['ownsSize'] for b in s['blocks']), 'attach claimed size ownership'
        assert not call('input',generation=generation,blockId=bid,text='NOT SENT')['ok']
        win=next(w for w in s['sessions'][0]['windows'] if w['activeBlockId']==bid or any(l['layout'] and bid in json.dumps(l['layout']) for l in w['layers']))
        assert ok('select',generation=s['generation'],sessionId=sid,windowId=win['id'])['ack']=='applied'
        wait(sentinel)
        assert command('detach-block',blockId=bid)['ack']=='applied'
        wait(lambda s:any(b['id']==bid and b['detached'] for b in s['blocks']))
        assert command('restore-block',blockId=bid,title='Restored')['ack']=='applied'
        wait(lambda s:any(b['id']==bid and not b['detached'] for b in s['blocks']))
        assert command('close-session',sessionId=sid)['ack']=='applied'
        wait(lambda s:not s['sessions'])
        print('PASS native bridge: empty attach, validation, authoritative create, input, decoded color frame, split, tab, reconnect, stale generation, select, detach, restore, close')
    except Exception:
        log.flush(); log.seek(0); print(log.read()[-18000:],file=sys.stderr)
        raise
    finally:
        if bridge and bridge.poll() is None:
            bridge.terminate(); bridge.wait(timeout=5)
        server.terminate()
        try: server.wait(timeout=5)
        except subprocess.TimeoutExpired: server.kill(); server.wait()
        log.close()
