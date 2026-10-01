"""Prove that the patched build tree matches the reviewed runtime source."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--source', type=Path, required=True)
parser.add_argument('--prepared', type=Path, required=True)
args = parser.parse_args()
manifest = json.loads((args.source / '.github/core-patches/series.json').read_text())
source = args.source.resolve()
prepared = args.prepared.resolve()
def git(root, *argv):
    return subprocess.check_output(['git', '-C', str(root), *argv])

# Publication workflows and their image probe are build orchestration, not core
# runtime patches. Everything else in the reviewed source must match exactly.
diff = git(source, 'diff', '--binary', manifest['upstream_revision'], 'HEAD', '--',
           '.', ':(exclude).github/**', ':(exclude)scripts/probe-agent-harness-image.sh')
with tempfile.TemporaryDirectory(prefix='reviewed-core-source-') as temp:
    checkout = Path(temp) / 'source'
    subprocess.run(['git', 'clone', '--quiet', '--no-hardlinks', '--no-checkout', str(prepared), str(checkout)], check=True)
    git(checkout, 'checkout', '--quiet', '--detach', manifest['upstream_revision'])
    subprocess.run(['git', '-C', str(checkout), 'apply', '--check', '--index', '--whitespace=error-all', '-'], input=diff, check=True)
    subprocess.run(['git', '-C', str(checkout), 'apply', '--index', '--whitespace=error-all', '-'], input=diff, check=True)
    tree = git(checkout, 'write-tree').decode().strip()
    if tree != manifest['result_tree'] or tree != git(prepared, 'write-tree').decode().strip():
        raise SystemExit('Patched upstream differs from the reviewed runtime source')
print('Patched official upstream exactly matches the reviewed runtime source')
