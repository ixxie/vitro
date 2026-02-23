#!/bin/sh
set -eu

REPO="ixxie/vitro"
INSTALL_DIR="${VITRO_INSTALL_DIR:-/usr/local/bin}"

detect_platform() {
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  arch=$(uname -m)

  case "$os" in
    linux) os="linux" ;;
    darwin) os="darwin" ;;
    *) echo "unsupported OS: $os" >&2; exit 1 ;;
  esac

  case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) echo "unsupported architecture: $arch" >&2; exit 1 ;;
  esac

  echo "vitro-${arch}-${os}"
}

main() {
  artifact=$(detect_platform)

  if [ -n "${VITRO_VERSION:-}" ]; then
    tag="v${VITRO_VERSION}"
    url="https://github.com/${REPO}/releases/download/${tag}/${artifact}.tar.gz"
  else
    url="https://github.com/${REPO}/releases/latest/download/${artifact}.tar.gz"
  fi

  echo "downloading ${url}..."
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT

  curl -fsSL "$url" -o "${tmp}/vitro.tar.gz"
  tar xzf "${tmp}/vitro.tar.gz" -C "$tmp"

  if [ -w "$INSTALL_DIR" ]; then
    mv "${tmp}/vitro" "${INSTALL_DIR}/vitro"
  else
    echo "installing to ${INSTALL_DIR} (requires sudo)..."
    sudo mv "${tmp}/vitro" "${INSTALL_DIR}/vitro"
  fi

  chmod +x "${INSTALL_DIR}/vitro"

  ln -sf "${INSTALL_DIR}/vitro" "${INSTALL_DIR}/git-remote-vitro"

  echo "installed vitro to ${INSTALL_DIR}/"
}

main
