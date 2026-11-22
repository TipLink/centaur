"""Prove that the patched build tree matches the reviewed runtime source."""
import argparse
import json
import subprocess
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--source', type=Path, required=True)
parser.add_argument('--prepared', type=Path, required=True)
args = parser.parse_args()
manifest = json.loads((args.source / '.github/core-patches/series.json').read_text())


def git(root, *argv):
    return subprocess.check_output(['git', '--no-optional-locks', '-C', str(root), *argv])


def runtime_entries(root, revision):
    # Root tree IDs cover every nested filename, mode and blob. Only the
    # publisher's .github directory may differ from the prepared upstream.
    return [entry for entry in git(root, 'ls-tree', '-z', revision).split(b'\0')
            if entry.partition(b'\t')[2] != b'.github']


prompt = 'services/sandbox/SYSTEM_PROMPT.md'
upstream = manifest['upstream_revision']
tree = manifest['result_tree']
if git(args.source, 'show', f'HEAD:{prompt}') != git(args.prepared, 'show', f'{upstream}:{prompt}'):
    raise SystemExit('Deployment guidance belongs in an overlay so the base prompt matches upstream')

# prepare-centaur-core.py applies the series to the index on a detached
# upstream HEAD. Check that index as well as the committed publisher sources.
git(args.prepared, 'diff', '--cached', '--quiet', tree, '--')
if git(args.prepared, 'rev-parse', f'{upstream}:.github') != git(args.prepared, 'rev-parse', f'{tree}:.github'):
    raise SystemExit('Core patches must preserve upstream publication workflows')
if runtime_entries(args.source, 'HEAD') != runtime_entries(args.prepared, tree):
    raise SystemExit('Patched upstream differs from the reviewed runtime source')
print('Patched official upstream exactly matches the reviewed runtime source')
