# Releasing to PyPI

The release approval point is a deliberate **stable `x.y.z` version increase**
merged to `main`, not a separate button press. Keep `pyproject.toml`, workspace
`Cargo.toml`, `Cargo.lock`, and `uv.lock` consistent in the reviewed change.
Do not bump the version merely to test the workflow: merging that bump can
publish the package automatically.

The existing `Publish to PyPI` workflow and PyPI OIDC trusted publisher are
retained; no package token is needed. It reacts when either `CI` or `Backend
integration` finishes for a same-repository push to `main`. The read-only gate
requires both latest runs for the **exact candidate SHA** and every expected
Rust, Python, and backend job to succeed. PR runs, forks, skipped jobs, a failed
or pending newer run, and missing/API-error evidence cannot authorize a release.
A version must increase relative to the candidate's first parent on main.
Unchanged versions, stale candidates, and incomplete tests skip preflight.

All wheels, the sdist, and installed-wheel smoke tests use the pinned candidate
SHA. All 15 wheel builds, the sdist, and smoke tests must finish before publishing.
The existing trusted-publishing job repeats the current-head and test checks
immediately before uploading. Runs are serialized without cancelling active
uploads. This is not an atomic lock on the main branch: main can still advance
just after the final check. The artifact identity guarantee is the immutable,
tested SHA, not an impossible guarantee that main never changes during upload.

## Retries and evidence

- A successful Release job includes verification that PyPI contains the complete
  16-file release with identical SHA256 hashes. Its source SHA, version, and
  artifact hashes are retained in the `release-manifest` run artifact. A later
  duplicate completion trigger for that same SHA skips the expensive build when
  an earlier Release job already succeeded.
- Immediately before upload, package metadata and the full 15-wheel + 1-sdist
  matrix are checked. Existing PyPI filenames must be a subset of this build and
  have identical hashes. Only missing files are uploaded; complete matching
  releases cause no upload. A version's mere existence is not considered proof
  that all files were published.
- For interrupted uploads, **re-run failed jobs** in the original release run to
  reuse its original artifacts. Matching partial uploads can then finish safely.
  A fresh whole-workflow rebuild may produce different bytes; differing hashes
  or unexpected PyPI files fail closed and require investigation, never overwrite.
- Registry verification waits up to 110 seconds for upload metadata propagation.
  A timeout leaves the Release job failed rather than claiming a complete release.
- Manual `workflow_dispatch` remains available on main and uses the same gates.
  It is not a way to bypass tests, publish an arbitrary branch, or release an
  unchanged-version commit. If main advanced after a failed release, investigate
  and prepare a new reviewed version-bump commit instead of publishing stale code.

The gate's offline tests are `python/tests/test_release_gate.py`, included in the
normal Python CI suites. Renaming workflows/jobs or changing the wheel matrix
requires updating the explicit gate expectations and tests together.
