#!/usr/bin/env bash
# Roadrunner release helper (.woodpecker/release.yaml); port of .github/workflows/release.yml.
#   bash ci/release.sh build vX.Y.Z    # host: dist/ from a rust container on the Dagger engine
#   GH_TOKEN=... bash ci/release.sh publish vX.Y.Z   # host: GitHub release from dist/
#   bash ci/release.sh dist vX.Y.Z     # in the container: tag check, zigbuild, package
# Tools are pinned by version + sha256 (dir: $RELEASE_TOOLS, see below).
set -euo pipefail

ZIG_VERSION=0.14.1        # mlugg/setup-zig version in release.yml
ZIGBUILD_VERSION=0.24.1   # `cargo install cargo-zigbuild` (latest) when ported
GH_VERSION=2.103.3
RUST_IMAGE=public.ecr.aws/docker/library/rust:1-bookworm
REPO=copper-browser/canvas-cloud
TARGETS="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu"

usage='usage: release.sh build|publish|dist vX.Y.Z'
cmd=${1:?$usage}
tag=${2:-}
cd "$(dirname "$0")/.."
# Host commands keep tools in the checkout; container commands outside /src (tests scan the tree).
case "$cmd" in
  build | publish) tools=${RELEASE_TOOLS:-$PWD/.release-tools} ;;
  *) tools=${RELEASE_TOOLS:-/tmp/release-tools} ;;
esac
mkdir -p "$tools"

case "$(uname -m)" in
  x86_64)
    gh_arch=amd64
    zig_sha=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c
    gh_sha=4c806978ffce1cec70853302a56d70ac6da4f19bae7adca4e1a8ab00633dd91d
    ;;
  aarch64)
    gh_arch=arm64
    zig_sha=f7a654acc967864f7a050ddacfaa778c7504a0eca8d2b678839c21eea47c992b
    gh_sha=ee32df2323c8847f5fac6cb6b4b69ca75828e9329a8f22f677021f7b3306ce63
    ;;
  *) echo "release.sh: unsupported machine $(uname -m)" >&2; exit 1 ;;
esac

# unpack <url> <sha256> <dir>: download into $tools, check the sha256, extract into $tools/<dir>.
unpack() {
  local file="$tools/${1##*/}"
  [ -f "$file" ] || curl -fsSL --retry 3 --connect-timeout 20 --max-time 600 -o "$file" "$1"
  echo "$2  $file" | sha256sum -c --quiet -
  rm -rf "${tools:?}/$3" && mkdir -p "$tools/$3"
  tar -xf "$file" -C "$tools/$3" --strip-components=1
}

need_tag() {
  [[ $tag == v* ]] || { echo "$usage (got tag '$tag')" >&2; exit 1; }
}

# package <rust target>: the archive + .sha256 sidecar, exactly as release.yml.
package() {
  local name="canvas-cloud-${version}-$1" stage
  stage="$(mktemp -d)/$name"
  mkdir -p "$stage/packaging" dist
  cp "${CARGO_TARGET_DIR:-target}/$1/release/canvas-cloud" "$stage/canvas-cloud"
  cp install.sh "$stage/install.sh"
  cp packaging/canvas-cloud.service "$stage/packaging/canvas-cloud.service"
  cp README.md "$stage/README.md"
  cp LICENSE "$stage/LICENSE"
  chmod 0755 "$stage/canvas-cloud" "$stage/install.sh"
  file "$stage/canvas-cloud"
  # Files at the archive root: `tar xzf … && sudo ./install.sh`.
  tar -C "$stage" -czf "dist/$name.tar.gz" canvas-cloud install.sh packaging README.md LICENSE
  (cd dist && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
  cat "dist/$name.tar.gz.sha256"
}

# In the rust container (or any Linux box with rustup): everything release.yml's build job did,
# plus its release job's SHA256SUMS (the per-archive .sha256 files are not published).
dist() {
  need_tag
  rustup toolchain install --no-self-update 2>/dev/null || rustup show active-toolchain
  version=$(cargo pkgid -p canvas-cloud)
  version=${version##*[#@]}
  [ "$version" = "${tag#v}" ] || { echo "tag $tag != crate version $version" >&2; exit 1; }
  unpack "https://ziglang.org/download/$ZIG_VERSION/zig-$(uname -m)-linux-$ZIG_VERSION.tar.xz" "$zig_sha" zig
  cargo install --locked -j4 --root "$tools/zigbuild" "cargo-zigbuild@$ZIGBUILD_VERSION"
  export PATH="$tools/zig:$tools/zigbuild/bin:$PATH"
  rm -rf dist
  for t in $TARGETS; do
    rustup target add "$t"
    cargo zigbuild --release --locked -j4 -p canvas-cloud --target "$t.2.28"
    package "$t"
  done
  (cd dist && cat ./*.sha256 > SHA256SUMS && rm ./*.sha256)
  ls -l dist
  cat dist/SHA256SUMS
}

# Host (engine VM): run dist() in a rust container through the persistent Dagger engine.
build() {
  need_tag
  rm -rf dist
  export _EXPERIMENTAL_DAGGER_RUNNER_HOST=${_EXPERIMENTAL_DAGGER_RUNNER_HOST:-unix:///run/dagger-engine/engine.sock}
  timeout 5400 dagger --progress=plain core container from --address="$RUST_IMAGE" \
    with-mounted-cache --path=/usr/local/cargo/registry --cache=canvas-cloud-cargo-registry \
    with-directory --path=/src --directory=. --exclude=.git,target,dist,.release-tools \
    with-workdir --path=/src \
    with-env-variable --name=CANVAS_CLOUD_GIT_SHA --value="${CI_COMMIT_SHA:-unknown}" \
    with-env-variable --name=CARGO_TERM_COLOR --value=never \
    with-exec --args=bash,ci/release.sh,dist,"$tag" \
    directory --path=/src/dist export --path=./dist
  ls -l dist
  cat dist/SHA256SUMS
}

# Host: upload dist/* (2 archives + SHA256SUMS) like softprops/action-gh-release with
# generate_release_notes; a re-run replaces the assets of an existing release.
publish() {
  need_tag
  : "${GH_TOKEN:?GH_TOKEN must be set}"
  if [ "$(find dist -name '*.tar.gz' | wc -l)" != 2 ] || [ ! -s dist/SHA256SUMS ]; then
    echo "dist/ incomplete" >&2
    exit 1
  fi
  unpack "https://github.com/cli/cli/releases/download/v$GH_VERSION/gh_${GH_VERSION}_linux_$gh_arch.tar.gz" "$gh_sha" gh
  export GH_CONFIG_DIR="$tools/gh-config" GH_PROMPT_DISABLED=1 GH_NO_UPDATE_NOTIFIER=1
  local gh="$tools/gh/bin/gh"
  if timeout 60 "$gh" release view "$tag" --repo "$REPO" >/dev/null 2>&1; then
    timeout 900 "$gh" release upload "$tag" dist/* --repo "$REPO" --clobber
  else
    timeout 900 "$gh" release create "$tag" dist/* --repo "$REPO" --verify-tag --title "$tag" --generate-notes
  fi
}

case "$cmd" in
  build | publish | dist) "$cmd" ;;
  *) echo "$usage" >&2; exit 1 ;;
esac
