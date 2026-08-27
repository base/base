# Depot CI configuration

The workflows in this directory require these Depot CI variables and secrets:

| Name | Kind | Purpose |
| --- | --- | --- |
| `DEPOT_PROJECT_ID` | GitHub and Depot CI variable | Depot container-build project used by `depot build` and `depot bake`. |
| `GHCR_USERNAME` | Variable | GitHub username that owns the GHCR personal access token. |
| `GHCR_TOKEN` | Secret | Classic GitHub PAT with `write:packages`, used to publish GHCR images. |

The `node` and `base-anvil` packages must grant the PAT owner access. Depot CI
cannot use its GitHub App token for GitHub Packages authentication.

Container builds use Depot's persistent project cache automatically. The Depot
workflows intentionally do not export Docker layer caches to GHCR.

Release publication remains in GitHub Actions because Depot CI does not provide
macOS sandboxes or GitHub artifact attestations. Its Docker build still runs on
Depot through `depot bake`.

GHCR cleanup remains in GitHub Actions so it can use the repository's short-lived
`GITHUB_TOKEN` instead of a PAT with package deletion access. Fork CI, release
orchestration, stale-issue handling, and the udeps issue reporter also remain in
GitHub Actions where Depot CI does not support the required event, permission, or
fork execution model.
