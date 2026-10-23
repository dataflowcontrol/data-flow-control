# Website publishing

Edit the website in `docs/` alongside the library code. The
`.github/workflows/publish-docs.yml` workflow copies this directory into
`docs/` on `main` in `dataflowcontrol/dataflowcontrol.github.io`. GitHub Pages
publishes that repository at https://dataflowcontrol.github.io/.

The same workflow publishes `pages-redirects/` as this repository's Pages
site. Known HTML pages redirect to their corresponding new URLs; the custom
404 page also maps old paths to the new site. JavaScript preserves query strings
and fragments. Without JavaScript, meta refresh and clickable links redirect
known pages, while the 404 page falls back to the new homepage. These are browser
redirects, not HTTP 301 responses; the custom 404 retains an HTTP 404 status.

## Initial setup (in order)

1. Commit and push the seeded `dataflowcontrol.github.io` checkout to `main`.
2. In that repository's **Settings → Pages**, choose **Deploy from a branch**,
   branch `main`, folder `/docs`. Leave the custom domain blank. Wait for the
   Pages deployment and check the homepage, examples, and documentation.
3. Create a fine-grained personal access token with resource owner
   `dataflowcontrol`, repository access limited to `dataflowcontrol.github.io`,
   and **Contents: Read and write**. Obtain organization approval if required.
   The token owner needs repository write access and permission to push to
   `main` under applicable rules. Set an expiry and renew before it expires.
4. In **this** repository, add the token under **Settings → Secrets and variables
   → Actions → New repository secret**, named `WEBSITE_PUBLISH_TOKEN`.
5. In **this** repository's **Settings → Pages**, change Source to
   **GitHub Actions**. Do this only after the new site is live.
6. Commit and push the publishing workflow, `pages-redirects/`, and this guide
   to this repository's `main`. In **Actions → Publish documentation**, inspect
   the run; use **Run workflow** on `main` to retry or publish manually.

Both repositories must allow GitHub Actions. If Actions are restricted, allow
the official checkout, configure-pages, upload-pages-artifact, and deploy-pages
actions used by the workflow. Any `github-pages` environment protection rules
must permit deployments from `main` (and may require a reviewer).

## Verification and maintenance

- Open https://dataflowcontrol.github.io/ and its `examples.html` and
  `documentation.html` pages. Check styles, images, and navigation.
- Open https://dataflowcontrol.github.io/data-flow-control/documentation.html?test=1#policy
  and confirm it lands at the new `/documentation.html?test=1#policy` URL.
- After the next docs edit, check the source workflow, the generated commit in
  the website repository, and its automatic Pages deployment. The source
  workflow confirms the push, but does not wait for the target Pages build.
- Do not edit the generated website repository's `docs/` files directly; sync
  replaces that directory, including deleting files removed from the source.
- Existing unrelated code changes do not trigger website publishing. Changes to
  `docs/`, `pages-redirects/`, or the publishing workflow do.

To roll back the old URL to serving the full docs, disable the publishing
workflow and switch this repository's Pages source back to **Deploy from a
branch → main → /docs**.
