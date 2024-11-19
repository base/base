#!/usr/bin/env bash

CURRENT_SOURE=${BASH_SOURCE[0]}
CURRENT_DIR=$(dirname $CURRENT_SOURE)
REPO_ROOT=$( cd $CURRENT_DIR >/dev/null 2>&1 && pwd )
REPO_ROOT=$(dirname $REPO_ROOT)
CHAINLIST_JSON=${REPO_ROOT}/superchain-registry/chainList.json
CONFIGS_JSON=${REPO_ROOT}/superchain-registry/superchain/configs/configs.json

# Attempt to copy over the chainList.json file to etc/chainList.json
if [ -f ${CHAINLIST_JSON} ]; then
    cp "${CHAINLIST_JSON}" "${REPO_ROOT}/etc/chainList.json"
else
    echo "[ERROR] ${CHAINLIST_JSON} does not exist"
    exit 1
fi

# Attempt to copy over the configs.json file to etc/configs.json
if [ -f ${CONFIGS_JSON} ]; then
    cp "${CONFIGS_JSON}" "${REPO_ROOT}/etc/configs.json"
else
    echo "[WARN] ${CONFIGS_JSON} does not exist"
    exit 1
fi
