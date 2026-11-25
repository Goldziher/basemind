#!/usr/bin/env bash
VERSION=1.2
export REGION="eu-west-1"
readonly ROOT=/srv/app
declare -x MODE=fast
A=1 B=2

log() {
  local level=$1
  echo "[$level] $2"
}

function deploy {
  TARGET=prod
  helper() { :; }
  log info "deploying"
}

function build_all() {
  make all
}

if [ -n "$CI" ]; then
  RUNNER=ci
  ci_only() { :; }
fi
