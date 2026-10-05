#!/usr/bin/env bash
# Contrato de verificacion. Este Mac no tiene Xcode: sin `metal`, gpui
# solo compila con los shaders en tiempo de ejecucion.
set -euo pipefail
cd "$(dirname "$0")/../.."
F=(--features gpui_platform/runtime_shaders)
case "${1:-}" in
  tipos) cargo check --quiet "${F[@]}" ;;
  humo|rapido|test) cargo test --quiet "${F[@]}" ;;
  *) exit 0 ;;
esac
