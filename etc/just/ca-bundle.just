# Shared custom CA bundle configuration for local Docker builds.
# Imported by etc/docker/Justfile, etc/just/prover.just, and crates/proof/tee/Justfile.
_ca_bundle := `if [ -n "${DOCKER_CA_BUNDLE:-}" ] && [ -f "$DOCKER_CA_BUNDLE" ]; then printf '%s' "$DOCKER_CA_BUNDLE"; fi`
_ca_bundle_secret := if _ca_bundle != "" { "--secret id=ca_bundle,src=" + quote(_ca_bundle) } else { "" }
