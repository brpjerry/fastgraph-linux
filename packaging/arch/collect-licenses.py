#!/usr/bin/env python3
"""Bundle notices from the exact frozen Cargo dependency graph."""
import json
from pathlib import Path
import shutil
import subprocess
import sys

# The workspace has no root package; the shipped binary is this member's.
BINARY_PACKAGE = 'fastgraph'
# Notices for crates that publish none, and for fonts that crates embed
# without one; see licenses/README.md.
LICENSES = Path(__file__).parent / 'licenses'
FALLBACKS = json.loads((LICENSES / 'fallbacks.json').read_text())


def packaged_dependencies(metadata):
    """Everything the binary's package resolves to through normal and build
    dependencies, skipping dev-only edges (test harnesses are not shipped).
    cargo metadata unifies features across the workspace, so this can still
    include a crate that only a test dependency's feature pulls in; an extra
    notice is harmless, a missing one is not."""
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    roots = [package['id'] for package in metadata['packages']
             if package['name'] == BINARY_PACKAGE
             and package['id'] in metadata['workspace_members']]
    if len(roots) != 1:
        raise ValueError(f'Expected one workspace member named {BINARY_PACKAGE}')
    reached, pending = set(), roots
    while pending:
        package = pending.pop()
        if package not in reached:
            reached.add(package)
            pending.extend(dep['pkg'] for dep in nodes[package]['deps']
                           if any(kind['kind'] != 'dev' for kind in dep['dep_kinds']))
    return reached


def collect(destination):
    metadata = json.loads(subprocess.check_output([
        'cargo', 'metadata', '--frozen', '--format-version=1',
        '--filter-platform', 'x86_64-unknown-linux-gnu'], text=True))
    destination.mkdir(parents=True, exist_ok=False)
    manifest = []
    resolved = packaged_dependencies(metadata)
    for package in metadata['packages']:
        if package['id'] not in resolved or package['id'] in metadata['workspace_members']:
            continue
        root = Path(package['manifest_path']).parent
        notices = set()
        for path in root.iterdir():
            if path.name.upper().startswith(('LICENSE', 'COPYING', 'NOTICE')):
                if path.is_file(): notices.add(path)
                elif path.is_dir(): notices.update(p for p in path.rglob('*') if p.is_file())
        if package.get('license_file'):
            notices.add(root / package['license_file'])
        folder = destination / f"{package['name']}-{package['version']}"
        # licenses/<group>/<file> is copied to <crate folder>/<file>, beside
        # any notices the crate publishes itself.
        files = FALLBACKS.get(f"{package['name']}-{package['version']}", [])
        if not notices and not files:
            raise ValueError(f"No bundled license notice found for {package['name']} {package['version']}")
        for name in files:
            target = folder / Path(name).relative_to(Path(name).parts[0])
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(LICENSES / name, target)
        for source in sorted(notices):
            relative = source.resolve().relative_to(root.resolve())
            target = folder / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
        manifest.append({key: package.get(key) for key in ('name', 'version', 'license', 'source')})
    (destination / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    collect(Path(sys.argv[1]))
