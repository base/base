# Reproduce the Nitro prover EIF

The canonical build exports `eif.bin`, PCR measurements and intermediate artifacts without publishing an image. `baseline.json` records measured artifact hashes and PCR0, with the exact source identity, Git tree and host evidence from those builds as provenance. This is a new reproducibility baseline. It is not a claim that the EIF matches the current onchain verifier. Updating this file does not authorize an onchain update or deployment.

## Supported clients and builders

Mac and Linux clients, on x86_64 or arm64, can submit the same build to an existing native Linux x86_64 Docker host. The build executes only on native Linux x86_64. Local arm64 emulation is not supported. The entry point checks the selected Docker daemon's OS and architecture before starting a build.

Clients need Git, Python 3.11 or newer, Docker with Buildx, and access to a native amd64 Docker daemon. CI uses Buildx v0.35.0. The script creates a fresh Docker-container builder with the checked-in BuildKit v0.23.2 amd64 digest, then removes it after the export. It does not remove other builders or their caches. Allow space for the Rust images, compiler output and exported artifacts on the builder.

On a native Linux x86_64 machine, use the existing local Docker context. On any supported client, select an existing remote Docker host over SSH:

```sh
docker context create nitro-native --docker host=ssh://USER@EXISTING_AMD64_HOST
export DOCKER_CONTEXT=nitro-native
docker info --format '{{.OSType}}/{{.Architecture}}'
```

The daemon must report `linux/x86_64` or `linux/amd64`. Docker commands and Buildx use the same selected context. The client does not need to run the EIF, Nitro CLI, Rust or linuxkit locally.

## Canonical commands

Run these commands at the repository root, from a checkout containing the build scripts for the revision you want to reproduce:

```sh
git rev-parse HEAD
python3 etc/docker/nitro-enclave/reproduce.py build --ref HEAD --output /tmp/nitro-first
```

The output directory must be absent or empty. `--ref` selects a committed Git revision or tree. Local edits, untracked files, checkout timestamps and filesystem ownership do not enter the build. Commit build changes before measuring them. You can also use `just tee::canonical-eif /tmp/nitro-first HEAD`.

Select a second, independent native amd64 Docker host and build the same revision:

```sh
export DOCKER_CONTEXT=SECOND_EXISTING_AMD64_CONTEXT
python3 etc/docker/nitro-enclave/reproduce.py build --ref HEAD --output /tmp/nitro-second
python3 etc/docker/nitro-enclave/reproduce.py compare \
  /tmp/nitro-first /tmp/nitro-second --require-independent \
  --baseline etc/docker/nitro-enclave/baseline.json
```

The two exports must have the same normalized source identity, full EIF bytes, EIF SHA256, PCR0 and every intermediate hash. `--require-independent` requires different worker hostnames and the canonical BuildKit pin. CI exports must also identify different jobs and attempt tuples. Docker daemon IDs are retained for diagnostics but are not unique across cloned runner images. Against `baseline.json`, EIF SHA256, PCR0 and every intermediate hash must match. The baseline's source identity and Git tree are measured provenance, not equality gates; source-context drift is reported diagnostically even when artifact bytes are unchanged.

Each export contains:

- `eif.bin` and `measurements.json`. The pinned AWS `eif_build` calculates PCR0 from the assembled image.
- The enclave binary, init and user ramdisks, bootstrap kernel and kernel configuration, command line and `eif_build` binary.
- `manifest.json` with SHA256 diagnostics, `identity.json` with the normalized context SHA256 and Git tree, and `host.json` with the worker hostname, Docker daemon ID, BuildKit pin and CI job/attempt evidence when present.
- BuildKit's export metadata. Host and BuildKit execution metadata do not enter the EIF or artifact hash comparison.

Use the first differing intermediate hash to locate a mismatch. An enclave-binary mismatch points to compilation inputs; a ramdisk-only mismatch points to packaging; an EIF-only mismatch points to assembly or identity metadata. Keep both exports and the build logs when investigating.

## Build inputs and metadata

The Dockerfile pins the amd64 Rust images, bootstrap image, Dockerfile frontend, linuxkit release checksum and AWS image-format commit. Both Cargo builds use checked-in lockfiles with `--locked`. Rust 1.96.0 is already in the pinned images; `RUSTUP_TOOLCHAIN` prevents the repository toolchain file from silently selecting another compiler download.

`apk.lock` contains SHA256 and URLs for the complete Alpine dependency closure, including upgrades to packages in the pinned base. `install-apks.sh` verifies every downloaded APK before installing it with networking and repository indexes disabled. The build does not resolve dependencies against a mutable APKINDEX. An upstream removal fails the build rather than selecting replacement bytes. When refreshing this lock, resolve the intended packages on the pinned amd64 Alpine base, download every selected direct and transitive APK, verify its Alpine signature, and record its SHA256 and exact URL. Review the entire closure and measure a new baseline.

The context comes from Git objects, with tar timestamps and ownership set to zero and file modes derived from Git. It excludes workflow directories, agent instructions, `docs/`, root Markdown, the docs indexes and manifest, this README, and `baseline.json`. Crate and binary READMEs remain because Rust uses them as compile-time inputs. The identity deliberately includes the rest of the tracked context, not only Rust files thought to affect the enclave. Unrelated repository or merge-queue changes can change this identity without changing artifacts. Both builds must still use the same normalized context; only artifact or PCR0 changes require a measured baseline update.

The enclave compiler remaps source and Cargo paths, disables incremental compilation and compiler wrappers, and remaps native compiler file paths. Artifact builds do not mount persistent Cargo target directories. Registry and Git download caches cannot supply compiled target output. `SOURCE_DATE_EPOCH` and EIF build-time are fixed.

linuxkit v1.8.2 already ignores source file stat metadata for these YAML `files` entries. It reads file contents, sets archive timestamps to `2017-04-18T16:30:00Z`, takes modes and ownership from YAML, and emits files in YAML order. Its CPIO writer assigns sequential inode numbers and its gzip header does not use the wall clock. Changing the YAML order can change bytes; changing host file mtimes, modes or inodes does not. `test-ramdisks.sh` checks actual ramdisk bytes after source paths, timestamps and modes change. Do not add timestamp rewriting to compensate for an unverified linuxkit defect.

The inline Justfile recipes use the same enclave compiler environment and fresh target directories. They still require their documented, preconfigured Alpine and Debian environments. Arbitrary inline environments and the runtime/describe images are not canonical baseline builds. Debian runtime and Nitro CLI packages are outside the EIF dependency chain. `just tee::verify-reproducible` compares two builds in one local environment; it does not prove independent hosts or enforce this measured baseline. Use the canonical commands for release measurements.

## CI and baseline updates

`.depot/workflows/nitro-eif-reproducibility.yml` builds on two separate existing native amd64 runners, using the default checkout revision for PRs and merge groups. Each uses a fresh pinned BuildKit instance and no layer cache. The jobs vary checkout timestamps and Rust file permissions, upload the full exports, and enforce the pair and baseline comparisons described above. The check rejects equal worker hostnames or CI job/attempt tuples. Match the worker hostnames to the CI service's separate job, attempt and sandbox records when reviewing independence. Client CPU architecture does not substitute for independent build-host evidence.

Exports use names such as `nitro-eif-first-attempt-1`. The comparator selects the highest numeric producing attempt separately for each matrix variant and verifies its recorded run, job and attempt against the export name. Artifact IDs and download order are not chronological evidence. The comparator job's own retry counter does not bound producer attempts. A full rerun selects both new exports; a partial rerun retains the successful variant's export and selects the retried variant's new export. The retained and retried exports must have the same normalized source identity. Missing exports, mismatched attempt evidence or differing artifacts fail the comparison rather than falling back to an older pair.

Update the baseline only for a reviewed change to artifact bytes or PCR0:

1. Commit the intended build inputs. Run the focused tests below. Do not change expected hashes to silence an unexplained mismatch.
2. Build that exact revision on two independent native amd64 hosts, using the canonical commands, or download the two successful CI exports.
3. Compare the exports without the old baseline, then generate a measured replacement:

   ```sh
   python3 etc/docker/nitro-enclave/reproduce.py compare \
     /tmp/nitro-first /tmp/nitro-second --require-independent
   python3 etc/docker/nitro-enclave/reproduce.py baseline \
     /tmp/nitro-first /tmp/nitro-second > /tmp/nitro-baseline.json
   cp /tmp/nitro-baseline.json etc/docker/nitro-enclave/baseline.json
   python3 etc/docker/nitro-enclave/reproduce.py compare \
     /tmp/nitro-first /tmp/nitro-second --require-independent \
     --baseline etc/docker/nitro-enclave/baseline.json
   ```

4. Review the source identity, EIF SHA256, PCR0 and intermediate changes. Commit the measured baseline with the input change and link both build-host records and exports in the PR. CI enforces the artifact hashes and PCR0, while preserving source and host records as measured provenance. `baseline.json` is excluded from the context, so adding the measured values does not change the build identity.

Changes that leave all artifacts and PCR0 unchanged do not require baseline refreshes. Keep the baseline's exact measured source and host records; do not relabel them as a later revision that was not built. Baseline generation refuses disagreeing source identities, artifacts or missing independent-host evidence. It never generates placeholder hashes. No update to the baseline changes the authorized onchain verification value.

Focused checks, without a full EIF build:

```sh
python3 etc/docker/nitro-enclave/test_reproducibility.py
sh etc/docker/nitro-enclave/test-build-env.sh etc/docker/nitro-enclave/build-env.sh
shellcheck etc/docker/nitro-enclave/{build-env,install-apks,test-build-env,test-ramdisks}.sh
python3 etc/scripts/local/docs-index.py check
```

The Python suite exercises Rust source-path bytes, committed-context metadata independence, retained compile-time includes, baseline-context-only drift, rejection of mismatched pair contexts and artifact/PCR0 changes, and producing-attempt selection across full and partial reruns. The Docker build also runs the pinned native Rust object check and ramdisk metadata regression. Full cross-host validation still requires two successful artifact exports and comparison; passing these focused checks alone is not release evidence.
