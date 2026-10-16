import importlib.util
import hashlib
import pathlib
import sys
import tempfile
import types
import unittest
from unittest.mock import patch, Mock

spec = importlib.util.spec_from_file_location('watch', pathlib.Path(__file__).with_name('watch.py'))
w = importlib.util.module_from_spec(spec)
spec.loader.exec_module(w)

class Remote:
    def __init__(self):
        self.files = {}; self.commands = []; self.uploads = []; self.corrupt = False
    def run(self, command, raw=False):
        self.commands.append((command, raw))
        if command.startswith('sha256sum '):
            b=self.files.get(command.split()[1]);return hashlib.sha256(b).hexdigest()+' file' if b is not None else 'not found'
        if command.startswith('mv '):
            _,src,dst=command.split();self.files[dst]=self.files.pop(src);return ''
        if command=='cat /proc/sys/kernel/random/boot_id':return '12345678-1234-1234-1234-123456789012'
        if command=='cat /proc/meminfo':return 'MemFree: 100000 kB'
        if 'tcp_tw_recycle' in command:return '0'
        return ''
    def upload(self, local, remote):
        b=pathlib.Path(local).read_bytes();self.files[remote]=b if not self.corrupt else b+b'x';self.uploads.append(remote);return hashlib.md5(b).hexdigest()
    def chmod_exec(self, path):self.commands.append(('chmod '+path,True))
    def close(self):pass

class TelnetGuardTests(unittest.TestCase):
    def test_only_exact_volatile_tcp_fix_is_authorized(self):
        sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
        from sharp_telnet import assert_safe
        assert_safe("echo 0 > /proc/sys/net/ipv4/tcp_tw_recycle")
        for command in ["echo 1 > /proc/sys/net/ipv4/tcp_tw_recycle",
                        "echo 0 > /proc/sys/net/ipv4/tcp_tw_reuse",
                        "echo 0 > /proc/sys/net/ipv4/tcp_tw_recycle; true",
                        "echo 0 > /etc/sysctl.conf"]:
            with self.assertRaises(RuntimeError):
                assert_safe(command)

class WatchTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.root=pathlib.Path(self.tmp.name);self.patches=[]
        for name,value in [('STATE_DIR',self.root),('STATE_FILE',self.root/'state.json'),('LOCK_FILE',self.root/'watch.lock'),('SCAN_LOCK',self.root/'scan.lock')]:self.set(name,value)
        binary=self.root/'mkra';binary.write_bytes(b'test binary');config=self.root/'channels.tsv';config.write_bytes(b'test channels')
        for name,value in [('BINARY',binary),('CONFIG',config),('EXPECTED_SIZE',binary.stat().st_size),('EXPECTED_SHA256',hashlib.sha256(binary.read_bytes()).hexdigest()),('EXPECTED_MD5',hashlib.md5(binary.read_bytes()).hexdigest()),('CONFIG_SHA256',hashlib.sha256(config.read_bytes()).hexdigest()),('CONFIG_MD5',hashlib.md5(config.read_bytes()).hexdigest())]:self.set(name,value)
    def set(self,name,value):
        p=patch.object(w,name,value);p.start();self.patches.append(p)
    def tearDown(self):
        for p in reversed(self.patches):p.stop()
        self.tmp.cleanup()
    def test_catalogue_rejects_old_4k_but_accepts_busy_unified(self):
        status={'process':{'pid':7,'versions':{'mkra':'0.1.0'}}};tuners=[{'index':0,'isAvailable':True,'isFault':False,'isUsing':True}]
        def fetch(ids):return lambda url:status if url.endswith('/status') else ([{'id':i} for i in ids] if url.endswith('/services') else tuners)
        self.assertTrue(w.api_healthy(fetch(w.REQUIRED_BS|{1100101}))[0]);self.assertFalse(w.api_healthy(fetch({1100101}))[0])
    def test_bundle_pins_include_channel_configuration(self):
        w.verify_bundle();w.CONFIG.write_bytes(b'wrong');self.assertRaises(RuntimeError,w.verify_bundle)
    def test_recording_and_schedule_safety(self):
        now=1800000000000
        self.assertFalse(w.parse_recordings_safe([], [{'id':1}],now)[0])
        self.assertFalse(w.parse_recordings_safe([{'startAt':now+100,'endAt':now+600000}],[],now)[0])
        self.assertFalse(w.parse_recordings_safe([{}],[],now)[0])
        self.assertTrue(w.parse_recordings_safe([{'isSkip':True,'startAt':now,'endAt':now+60000}],[],now)[0])
    def test_atomic_pinned_upload_and_idempotent_reuse(self):
        r=Remote()
        with patch.object(w,'read_only_device_state',return_value=(True,[])),patch.object(w,'local_start_safety',return_value=(True,'idle')):
            self.assertTrue(w.deploy_files(r,r.run('cat /proc/sys/kernel/random/boot_id'))[0]);self.assertEqual(len(r.uploads),2)
            self.assertEqual(r.files[w.REMOTE],w.BINARY.read_bytes());self.assertEqual(r.files[w.REMOTE_CONFIG],w.CONFIG.read_bytes())
            self.assertTrue(w.deploy_files(r,r.run('cat /proc/sys/kernel/random/boot_id'))[0]);self.assertEqual(len(r.uploads),2)
    def test_corrupt_transfer_never_installs(self):
        r=Remote();r.corrupt=True
        self.assertRaises(RuntimeError,w.deploy_files,r,r.run('cat /proc/sys/kernel/random/boot_id'));self.assertNotIn(w.REMOTE,r.files)
    def test_reboot_during_transfer_never_installs(self):
        r=Remote();self.assertRaises(RuntimeError,w.deploy_files,r,'different-boot');self.assertNotIn(w.REMOTE,r.files)
    def test_new_process_or_recording_during_upload_never_installs(self):
        for running,safe in [(['other mkra'],True),([],False)]:
            r=Remote()
            with patch.object(w,'read_only_device_state',return_value=(True,running)),patch.object(w,'local_start_safety',return_value=(safe,'busy')):
                if running:self.assertRaises(RuntimeError,w.deploy_files,r,r.run('cat /proc/sys/kernel/random/boot_id'))
                else:self.assertFalse(w.deploy_files(r,r.run('cat /proc/sys/kernel/random/boot_id'))[0])
                self.assertNotIn(w.REMOTE,r.files)
    def test_healthy_run_never_connects_or_uploads(self):
        with patch.object(w,'api_healthy',return_value=(True,'healthy')),patch.object(w,'verify_proxy_and_epgstation',return_value='ok'),patch.object(w,'restore') as restore:
            self.assertEqual(w.run_once(),0);restore.assert_not_called()
    def test_watch_and_scan_locks_prevent_restore(self):
        for path in [w.LOCK_FILE,w.SCAN_LOCK]:
            with path.open('a') as held:
                w.fcntl.flock(held,w.fcntl.LOCK_EX|w.fcntl.LOCK_NB)
                with patch.object(w,'api_healthy',return_value=(False,'down')),patch.object(w,'restore') as restore:
                    w.run_once();restore.assert_not_called()
            w.STATE_FILE.unlink(missing_ok=True)
    def test_existing_process_standby_and_recording_never_write(self):
        for device,safe in [((True,['running']),True),((False,[]),True),((True,[]),False)]:
            r=Remote();module=types.SimpleNamespace(connect=Mock(return_value=r))
            with patch.dict(sys.modules,{'sharp_telnet':module}),patch.object(w,'read_only_device_state',return_value=device),patch.object(w,'local_start_safety',return_value=(safe,'recording')):
                w.restore({},'down');self.assertFalse(r.uploads);self.assertNotIn((w.START_COMMAND,False),r.commands)
    def test_successful_full_restore_and_dry_run(self):
        for dry in [False,True]:
            r=Remote();module=types.SimpleNamespace(connect=Mock(return_value=r))
            with patch.dict(sys.modules,{'sharp_telnet':module}),patch.object(w,'read_only_device_state',return_value=(True,[])),patch.object(w,'local_start_safety',return_value=(True,'idle')),patch.object(w,'api_healthy',return_value=(True,'ready')),patch.object(w,'verify_proxy_and_epgstation',return_value='ok'):
                w.restore({},'down',dry_run=dry);self.assertEqual(bool(r.uploads),not dry);self.assertEqual((w.START_COMMAND,False) in r.commands,not dry)
    def test_native_services_allow_headless_and_require_all(self):
        r=Remote()
        def run(c):
            if c.startswith('pidof '):return '11\n12\n13\n14'
            if c.startswith('test -c'):return 'KARIN_NATIVE_READY'
            if c=='cat /proc/uptime':return '100 90'
            return ''
        r.run=run;self.assertTrue(w.read_only_device_state(r)[0])
        r.run=lambda c: '11\n12\n13' if c.startswith('pidof ') else run(c)
        self.assertFalse(w.read_only_device_state(r)[0])
        r.run=lambda c: '10 5' if c=='cat /proc/uptime' else run(c)
        self.assertFalse(w.read_only_device_state(r)[0])
    def test_malformed_recording_api_fails_closed(self):
        with patch.object(w,'get_json',return_value={}):self.assertFalse(w.local_start_safety()[0])
    def test_missing_endpoint_can_bootstrap_before_an_imminent_reservation(self):
        now=w.time.time()*1000
        data=[{'reserves':[{'startAt':now+1000,'endAt':now+60000}]},{'records':[]}]
        inactive=types.SimpleNamespace(returncode=0,stdout='inactive')
        with patch.object(w,'get_json',side_effect=data),patch.object(w.subprocess,'run',return_value=inactive):self.assertTrue(w.local_start_safety()[0])
        with patch.object(w,'get_json',side_effect=data),patch.object(w.subprocess,'run',return_value=inactive):self.assertFalse(w.local_start_safety(require_reservation_guard=True)[0])
    def test_either_epg_scanner_blocks_restore(self):
        for target in ['bs2k-epg-scan.service','bs4k-epg-scan.service']:
            def systemctl(argv,**kw):return types.SimpleNamespace(returncode=0,stdout='activating' if argv[-1]==target else 'inactive')
            with patch.object(w,'get_json',side_effect=[{'reserves':[]},{'records':[]}]),patch.object(w.subprocess,'run',side_effect=systemctl):self.assertFalse(w.local_start_safety()[0])

if __name__=='__main__':unittest.main()
