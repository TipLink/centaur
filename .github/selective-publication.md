# Testing selective image publication

To preview a release, fetch the candidate and the previous signed publication head
into the publisher checkout. Run the planner from the checkout containing the
selector. It reads previous image indexes and BuildKit provenance from public
registries. The two arguments are full commit SHAs.

```sh
python3 .github/scripts/plan-reviewed-images.py --previous "$PREVIOUS" --current "$CANDIDATE"
```

The JSON lists the services and architectures to build and the exact index and
ARM child digests to reuse. With no previous release, every service is built.
Infra retains the original workload digest of each reused image.

The image validation workflow’s release-policy job runs the disposable registry
test on release/aws-core PRs. It builds Code Mode and agent fixture images for both architectures,
runs the mixed release and deliberately changes shared inputs. Re-run that job
to repeat the test without publishing packages.

A reviewed publication uses the existing signed-head tag or dispatch workflow.
Use the full_rebuild dispatch option to refresh unversioned network downloads
within RUN commands. After publication, lock the images in infra using the
successful run URL and exact signed publisher head.

The AWS descriptor covers eight components. Its `codemode` row points to
`ghcr.io/tiplink/centaur/centaur-codemode`. Both PR validation and publication
build Code Mode on linux/amd64 and linux/arm64 from the prepared repository root,
using the final stage of `services/codemode/Dockerfile`. BuildKit records the
workspace Cargo manifest and lockfile, `crates/`, and `connectors/` as inputs.
Connector changes also affect the agent image. Changing the upstream revision
requires a full build across the publisher's 13 services.

The descriptor records the runnable ARM child digest. The AWS lock generator
resolves the reviewed multi-platform index and checks each platform's config
labels for the official source and revision, patch manifest hash, result tree,
and original publication commit. The initial tag-absence gate requires a
registry `MANIFEST_UNKNOWN` response for every image, including Code Mode.
