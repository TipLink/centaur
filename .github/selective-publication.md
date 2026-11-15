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
test on release/aws-core PRs. It builds two real services for both architectures,
runs the mixed release and deliberately changes shared inputs. Re-run that job
to repeat the test without publishing packages.

A reviewed publication uses the existing signed-head tag or dispatch workflow.
Use the full_rebuild dispatch option to refresh unversioned network downloads
within RUN commands. After publication, lock the images in infra using the
successful run URL and exact signed publisher head.
