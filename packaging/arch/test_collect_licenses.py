"""Check which crates get notices, without Cargo or network access."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).parent
spec = importlib.util.spec_from_file_location('collect_licenses', HERE / 'collect-licenses.py')
collect_licenses = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collect_licenses)


def dep(pkg, *kinds):
    return {'pkg': pkg, 'dep_kinds': [{'kind': kind} for kind in kinds]}


class CollectLicensesTests(unittest.TestCase):
    def metadata(self, packages, nodes):
        return {
            'packages': packages,
            'workspace_members': ['app', 'core'],
            'resolve': {'root': None, 'nodes': nodes},
        }

    # Bugs caught: 0, False positives: 0
    def test_dev_only_dependencies_are_not_shipped(self):
        metadata = self.metadata(
            [{'id': 'app', 'name': 'fastgraph'}, {'id': 'core', 'name': 'fastgraph-core'}],
            [{'id': 'app', 'deps': [dep('core', None), dep('harness', 'dev'), dep('both', 'dev', None)]},
             {'id': 'core', 'deps': [dep('codegen', 'build')]},
             {'id': 'harness', 'deps': [dep('harness-only', None)]},
             {'id': 'both', 'deps': []}, {'id': 'codegen', 'deps': []},
             {'id': 'harness-only', 'deps': []}])
        self.assertEqual(collect_licenses.packaged_dependencies(metadata),
                         {'app', 'core', 'both', 'codegen'})

    # Bugs caught: 0, False positives: 0
    def test_missing_binary_package_fails(self):
        metadata = self.metadata([{'id': 'core', 'name': 'fastgraph-core'}],
                                 [{'id': 'core', 'deps': []}])
        with self.assertRaisesRegex(ValueError, 'workspace member'):
            collect_licenses.packaged_dependencies(metadata)

    # Bugs caught: 0, False positives: 0
    def test_every_fallback_notice_exists_and_is_documented(self):
        documented = (HERE / 'licenses/README.md').read_text()
        for crate, files in collect_licenses.FALLBACKS.items():
            self.assertTrue(files, crate)
            for name in files:
                self.assertTrue((HERE / 'licenses' / name).is_file(), name)
                self.assertIn(f'`{name.split("/")[0]}/', documented, name)

    # Bugs caught: 0, False positives: 0
    def test_fallback_copied_only_for_exact_version(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name, version in (('iced_core', '0.14.0'), ('iced_core', '0.14.9')):
                crate = root / f'{name}-{version}'
                crate.mkdir()
                (crate / 'Cargo.toml').write_text('')
            packages = [
                {'id': 'app', 'name': 'fastgraph', 'version': '0.1.0', 'manifest_path': '/unused'},
                {'id': 'known', 'name': 'iced_core', 'version': '0.14.0', 'license': 'MIT',
                 'source': 'registry', 'manifest_path': str(root / 'iced_core-0.14.0/Cargo.toml')},
                {'id': 'unknown', 'name': 'iced_core', 'version': '0.14.9', 'license': 'MIT',
                 'source': 'registry', 'manifest_path': str(root / 'iced_core-0.14.9/Cargo.toml')},
            ]
            metadata = self.metadata(packages, [{'id': 'app', 'deps': [dep('known', None)]},
                                                {'id': 'known', 'deps': []}])
            metadata['workspace_members'] = ['app']
            with patch.object(collect_licenses.subprocess, 'check_output', return_value=json.dumps(metadata)):
                collect_licenses.collect(root / 'out')
            self.assertEqual((root / 'out/iced_core-0.14.0/LICENSE').read_bytes(),
                             (HERE / 'licenses/iced/LICENSE').read_bytes())
            manifest = json.loads((root / 'out/manifest.json').read_text())
            self.assertEqual([entry['version'] for entry in manifest], ['0.14.0'])

            metadata['resolve']['nodes'][0]['deps'] = [dep('unknown', None)]
            metadata['resolve']['nodes'].append({'id': 'unknown', 'deps': []})
            with patch.object(collect_licenses.subprocess, 'check_output', return_value=json.dumps(metadata)):
                with self.assertRaisesRegex(ValueError, 'iced_core 0.14.9'):
                    collect_licenses.collect(root / 'refused')

    # Bugs caught: 0, False positives: 0
    def test_font_notice_is_added_beside_the_crates_own_license(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            crate = root / 'sctk-adwaita-0.10.1'
            crate.mkdir()
            (crate / 'Cargo.toml').write_text('')
            (crate / 'LICENSE').write_text('crate notice\n')
            packages = [
                {'id': 'app', 'name': 'fastgraph', 'version': '0.1.0', 'manifest_path': '/unused'},
                {'id': 'csd', 'name': 'sctk-adwaita', 'version': '0.10.1', 'license': 'MIT',
                 'source': 'registry', 'manifest_path': str(crate / 'Cargo.toml')},
            ]
            metadata = self.metadata(packages, [{'id': 'app', 'deps': [dep('csd', None)]},
                                                {'id': 'csd', 'deps': []}])
            metadata['workspace_members'] = ['app']
            with patch.object(collect_licenses.subprocess, 'check_output', return_value=json.dumps(metadata)):
                collect_licenses.collect(root / 'out')
            out = root / 'out/sctk-adwaita-0.10.1'
            self.assertEqual((out / 'LICENSE').read_text(), 'crate notice\n')
            self.assertEqual((out / 'Cantarell-OFL.txt').read_bytes(),
                             (HERE / 'licenses/sctk-adwaita/Cantarell-OFL.txt').read_bytes())

    # Bugs caught: 0, False positives: 0
    def test_font_notices_name_their_copyright_holders(self):
        for name, holder in (('iced/FiraSans-OFL.txt', 'The Mozilla Foundation'),
                             ('iced/Iced-Icons-OFL.txt', 'Dave Gandy'),
                             ('sctk-adwaita/Cantarell-OFL.txt', 'Understanding Limited')):
            text = (HERE / 'licenses' / name).read_text()
            self.assertIn(holder, text, name)
            self.assertIn('SIL OPEN FONT LICENSE Version 1.1', text, name)


if __name__ == '__main__':
    unittest.main()
