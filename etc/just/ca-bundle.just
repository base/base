# Shared custom CA bundle configuration for local Docker builds.
# Imported by etc/docker/Justfile, etc/just/prover.just, and crates/proof/tee/Justfile.
_ca_bundle_raw := env("DOCKER_CA_BUNDLE", "")
_ca_bundle := if _ca_bundle_raw != "" {
    shell(
        'cd "$1" && if [ -f "$2" ]; then cd "$(dirname "$2")" && printf "%s/%s" "$(pwd -P)" "$(basename "$2")"; else printf "warning: DOCKER_CA_BUNDLE=%s is not a regular file; building without the ca_bundle secret\n" "$2" >&2; fi',
        invocation_directory(),
        _ca_bundle_raw,
    )
} else {
    ""
}
_ca_bundle_secret := if _ca_bundle != "" { "--secret id=ca_bundle,src=" + quote(_ca_bundle) } else { "" }
_ca_bundle_bake_allow := if _ca_bundle != "" { "--allow=fs.read=" + quote(_ca_bundle) } else { "" }
