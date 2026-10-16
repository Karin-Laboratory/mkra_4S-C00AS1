import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('make_bundle', Path(__file__).with_name('make_bundle.py'))
bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundle)

class BundleTests(unittest.TestCase):
    def test_rejects_non_arm_without_creating_destination(self):
        with tempfile.TemporaryDirectory() as tmp:
            source, dest = Path(tmp)/'input', Path(tmp)/'output'
            source.write_bytes(b'not an ELF')
            with self.assertRaises(ValueError): bundle.make_bundle(source, dest)
            self.assertFalse(dest.exists())

    def test_manifest_detects_changes_and_destination_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as tmp:
            source, dest = Path(tmp)/'input', Path(tmp)/'output'
            data = bytearray(64); data[:7] = b'\x7fELF\x01\x01\x01'; data[18] = 40
            source.write_bytes(data)
            manifest = bundle.make_bundle(source, dest)
            self.assertEqual(manifest['binary_size'], 64)
            self.assertEqual((dest/'mkra').read_bytes(), data)
            watch_spec = importlib.util.spec_from_file_location('generated_watch', dest/'watch.py')
            watch = importlib.util.module_from_spec(watch_spec); watch_spec.loader.exec_module(watch)
            watch.verify_bundle()
            (dest/'mkra').write_bytes(data + b'changed')
            with self.assertRaises(RuntimeError): watch.verify_bundle()
            with self.assertRaises(FileExistsError): bundle.make_bundle(source, dest)
