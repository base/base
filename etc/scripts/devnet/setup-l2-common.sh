#!/bin/bash

setup_l2_common_load_defaults() {
  L2_BASE_AZUL_BLOCK="${L2_BASE_AZUL_BLOCK:-}"
  L2_BASE_BERYL_BLOCK="${L2_BASE_BERYL_BLOCK:-}"
  L2_ISTHMUS_BLOCK="${L2_ISTHMUS_BLOCK:-}"
  L2_BASE_COBALT_BLOCK="${L2_BASE_COBALT_BLOCK:-}"
  L2_ACTIVATION_ADMIN_ADDR="${L2_ACTIVATION_ADMIN_ADDR:-${SEQUENCER_ADDR:-}}"
  L2_EL_BOOTNODE_P2P_KEY="${L2_EL_BOOTNODE_P2P_KEY:-1111111111111111111111111111111111111111111111111111111111111111}"
  L2_EL_BOOTNODE_ENODE_ID="${L2_EL_BOOTNODE_ENODE_ID:-4f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa385b6b1b8ead809ca67454d9683fcf2ba03456d6fe2c4abe2b07f0fbdbb2f1c1}"
  L2_EL_BOOTNODE_ENODE="${L2_EL_BOOTNODE_ENODE:-enode://4f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa385b6b1b8ead809ca67454d9683fcf2ba03456d6fe2c4abe2b07f0fbdbb2f1c1@172.30.0.10:9303}"
  L2_CL_BOOTNODE_P2P_KEY="${L2_CL_BOOTNODE_P2P_KEY:-2222222222222222222222222222222222222222222222222222222222222222}"
  L2_CL_BOOTNODE_ENR_PATH="${L2_CL_BOOTNODE_ENR_PATH:-/bootnodes/cl-bootnode.enr}"
}

setup_l2_common_replace_output_file() {
  local source_file="$1"
  local destination_file="$2"

  chmod 0644 "$source_file"
  mv "$source_file" "$destination_file"
}

setup_l2_common_validate_activations() {
  if [ -n "$L2_BASE_AZUL_BLOCK" ] && ! [[ "$L2_BASE_AZUL_BLOCK" =~ ^[0-9]+$ ]]; then
    echo "ERROR: L2_BASE_AZUL_BLOCK must be a non-negative integer when set, got: $L2_BASE_AZUL_BLOCK"
    exit 1
  fi
  if [ -n "$L2_BASE_BERYL_BLOCK" ] && ! [[ "$L2_BASE_BERYL_BLOCK" =~ ^[0-9]+$ ]]; then
    echo "ERROR: L2_BASE_BERYL_BLOCK must be a non-negative integer when set, got: $L2_BASE_BERYL_BLOCK"
    exit 1
  fi
  if [ -n "$L2_BASE_COBALT_BLOCK" ] && ! [[ "$L2_BASE_COBALT_BLOCK" =~ ^[0-9]+$ ]]; then
    echo "ERROR: L2_BASE_COBALT_BLOCK must be a non-negative integer when set, got: $L2_BASE_COBALT_BLOCK"
    exit 1
  fi
  if [ -n "$L2_ISTHMUS_BLOCK" ] && ! [[ "$L2_ISTHMUS_BLOCK" =~ ^[0-9]+$ ]]; then
    echo "ERROR: L2_ISTHMUS_BLOCK must be a non-negative integer when set, got: $L2_ISTHMUS_BLOCK"
    exit 1
  fi
}

setup_l2_common_print_activation_config() {
  echo "Activation admin address: $L2_ACTIVATION_ADMIN_ADDR"
  if [ -n "$L2_BASE_AZUL_BLOCK" ]; then
    echo "Base Azul activation block: $L2_BASE_AZUL_BLOCK"
  else
    echo "Base Azul activation block: <unset>"
  fi
  if [ -n "$L2_BASE_BERYL_BLOCK" ]; then
    echo "Base Beryl activation block: $L2_BASE_BERYL_BLOCK"
  else
    echo "Base Beryl activation block: <unset>"
  fi
  if [ -n "$L2_BASE_COBALT_BLOCK" ]; then
    echo "Base Cobalt activation block: $L2_BASE_COBALT_BLOCK"
  else
    echo "Base Cobalt activation block: <unset>"
  fi
  if [ -n "$L2_ISTHMUS_BLOCK" ]; then
    echo "Isthmus activation block: $L2_ISTHMUS_BLOCK"
  else
    echo "Isthmus activation block: <unset>"
  fi
}

setup_l2_common_patch_artifacts() {
  local output_dir="$1"
  local tmp_genesis
  local tmp_rollup
  local l2_block_time
  local l2_genesis_time
  local activation_time

  tmp_genesis=$(mktemp)
  jq \
    --arg activation_admin "$L2_ACTIVATION_ADMIN_ADDR" \
    '.config.activationAdminAddress = $activation_admin' \
    "$output_dir/genesis.json" \
    >"$tmp_genesis"
  setup_l2_common_replace_output_file "$tmp_genesis" "$output_dir/genesis.json"
  echo "Patched activation admin into genesis config"

  l2_block_time=$(jq -re '.block_time' "$output_dir/rollup.json")
  l2_genesis_time=$(jq -re '.genesis.l2_time' "$output_dir/rollup.json")

  if [ -n "$L2_ISTHMUS_BLOCK" ]; then
    activation_time=$((l2_genesis_time + l2_block_time * L2_ISTHMUS_BLOCK))

    echo ""
    echo "=== Configuring Isthmus Activation ==="
    echo "L2 genesis time: $l2_genesis_time"
    echo "L2 block time: $l2_block_time"
    echo "Isthmus activation block: $L2_ISTHMUS_BLOCK"
    echo "Derived Isthmus activation timestamp: $activation_time"

    tmp_rollup=$(mktemp)
    jq \
      --argjson isthmus_time "$activation_time" \
      '.isthmus_time = $isthmus_time' \
      "$output_dir/rollup.json" \
      >"$tmp_rollup"
    setup_l2_common_replace_output_file "$tmp_rollup" "$output_dir/rollup.json"

    tmp_genesis=$(mktemp)
    jq \
      --argjson isthmus_time "$activation_time" \
      '.config.isthmusTime = $isthmus_time' \
      "$output_dir/genesis.json" \
      >"$tmp_genesis"
    setup_l2_common_replace_output_file "$tmp_genesis" "$output_dir/genesis.json"

    echo "Patched Isthmus activation into rollup and genesis configs"
  else
    echo ""
    echo "=== Configuring Isthmus Activation ==="
    echo "L2 genesis time: $l2_genesis_time"
    echo "L2 block time: $l2_block_time"
    echo "Isthmus activation block is unset; leaving isthmus_time and isthmusTime unchanged"
  fi

  if [ -n "$L2_BASE_AZUL_BLOCK" ]; then
    activation_time=$((l2_genesis_time + l2_block_time * L2_BASE_AZUL_BLOCK))

    echo ""
    echo "=== Configuring Base Azul Activation ==="
    echo "L2 genesis time: $l2_genesis_time"
    echo "L2 block time: $l2_block_time"
    echo "Base Azul activation block: $L2_BASE_AZUL_BLOCK"
    echo "Derived Base Azul activation timestamp: $activation_time"

    tmp_rollup=$(mktemp)
    jq \
      --argjson azul_time "$activation_time" \
      '.base = ((.base // {}) + {azul: $azul_time})' \
      "$output_dir/rollup.json" \
      >"$tmp_rollup"
    setup_l2_common_replace_output_file "$tmp_rollup" "$output_dir/rollup.json"

    tmp_genesis=$(mktemp)
    jq \
      --argjson azul_time "$activation_time" \
      '.config.osakaTime = $azul_time
      | .config.base = ((.config.base // {}) + {azul: $azul_time})' \
      "$output_dir/genesis.json" \
      >"$tmp_genesis"
    setup_l2_common_replace_output_file "$tmp_genesis" "$output_dir/genesis.json"

    echo "Patched Base Azul activation into rollup and genesis configs"
  else
    echo ""
    echo "=== Configuring Base Azul Activation ==="
    echo "L2 genesis time: $l2_genesis_time"
    echo "L2 block time: $l2_block_time"
    echo "Base Azul activation block is unset; leaving base.azul and osakaTime unchanged"
  fi

  if [ -n "$L2_BASE_BERYL_BLOCK" ]; then
    activation_time=$((l2_genesis_time + l2_block_time * L2_BASE_BERYL_BLOCK))

    echo ""
    echo "=== Configuring Base Beryl Activation ==="
    echo "L2 genesis time: $l2_genesis_time"
    echo "L2 block time: $l2_block_time"
    echo "Base Beryl activation block: $L2_BASE_BERYL_BLOCK"
    echo "Derived Base Beryl activation timestamp: $activation_time"

    tmp_rollup=$(mktemp)
    jq \
      --argjson beryl_time "$activation_time" \
      '.base = ((.base // {}) + {beryl: $beryl_time})' \
      "$output_dir/rollup.json" \
      >"$tmp_rollup"
    setup_l2_common_replace_output_file "$tmp_rollup" "$output_dir/rollup.json"

    tmp_genesis=$(mktemp)
    jq \
      --argjson beryl_time "$activation_time" \
      '.config.base = ((.config.base // {}) + {beryl: $beryl_time})' \
      "$output_dir/genesis.json" \
      >"$tmp_genesis"
    setup_l2_common_replace_output_file "$tmp_genesis" "$output_dir/genesis.json"

    echo "Patched Base Beryl activation into rollup and genesis configs"
  else
    echo ""
    echo "=== Configuring Base Beryl Activation ==="
    echo "L2 genesis time: $l2_genesis_time"
    echo "L2 block time: $l2_block_time"
    echo "Base Beryl activation block is unset; leaving base.beryl unchanged"
  fi

  if [ -n "$L2_BASE_COBALT_BLOCK" ]; then
    activation_time=$((l2_genesis_time + l2_block_time * L2_BASE_COBALT_BLOCK))

    echo ""
    echo "=== Configuring Base Cobalt Activation ==="
    echo "L2 genesis time: $l2_genesis_time"
    echo "L2 block time: $l2_block_time"
    echo "Base Cobalt activation block: $L2_BASE_COBALT_BLOCK"
    echo "Derived Base Cobalt activation timestamp: $activation_time"

    tmp_rollup=$(mktemp)
    jq \
      --argjson cobalt_time "$activation_time" \
      '.base = ((.base // {}) + {cobalt: $cobalt_time})' \
      "$output_dir/rollup.json" \
      >"$tmp_rollup"
    setup_l2_common_replace_output_file "$tmp_rollup" "$output_dir/rollup.json"

    tmp_genesis=$(mktemp)
    jq \
      --argjson cobalt_time "$activation_time" \
      '.config.base = ((.config.base // {}) + {cobalt: $cobalt_time})' \
      "$output_dir/genesis.json" \
      >"$tmp_genesis"
    setup_l2_common_replace_output_file "$tmp_genesis" "$output_dir/genesis.json"

    echo "Patched Base Cobalt activation into rollup and genesis configs"
  else
    echo ""
    echo "=== Configuring Base Cobalt Activation ==="
    echo "L2 genesis time: $l2_genesis_time"
    echo "L2 block time: $l2_block_time"
    echo "Base Cobalt activation block is unset; leaving base.cobalt unchanged"
  fi
}

setup_l2_common_write_rollup_conductor_config() {
  local output_dir="$1"

  echo "Writing rollup-conductor.json (base fields stripped for op-conductor compatibility)..."
  jq 'del(.base)' "$output_dir/rollup.json" >"$output_dir/rollup-conductor.json"
  echo "rollup-conductor.json written to $output_dir/rollup-conductor.json"
}

setup_l2_common_write_p2p_keys() {
  local output_dir="$1"

  echo ""
  echo "=== Generating P2P Keys ==="

  echo "$BUILDER_P2P_KEY" >"$output_dir/builder-p2p-key.txt"
  echo "$BUILDER_ENODE_ID" >"$output_dir/builder-enode-id.txt"
  printf "%s" "$L2_EL_BOOTNODE_P2P_KEY" >"$output_dir/el-bootnode-p2p-key.txt"
  echo "$L2_EL_BOOTNODE_ENODE_ID" >"$output_dir/el-bootnode-enode-id.txt"
  echo "$L2_EL_BOOTNODE_ENODE" >"$output_dir/el-bootnode-enode.txt"
  printf "%s" "$L2_CL_BOOTNODE_P2P_KEY" >"$output_dir/cl-bootnode-p2p-key.txt"
  echo "$L2_CL_BOOTNODE_ENR_PATH" >"$output_dir/cl-bootnode-enr-path.txt"
  echo "$SEQ1_P2P_KEY" >"$output_dir/sequencer-1-p2p-key.txt"
  echo "$SEQ2_P2P_KEY" >"$output_dir/sequencer-2-p2p-key.txt"

  echo "Builder P2P key written to $output_dir/builder-p2p-key.txt"
  echo "Builder enode ID: $BUILDER_ENODE_ID"
  echo "EL bootnode P2P key written to $output_dir/el-bootnode-p2p-key.txt"
  echo "EL bootnode enode: $L2_EL_BOOTNODE_ENODE"
  echo "CL bootnode P2P key written to $output_dir/cl-bootnode-p2p-key.txt"
  echo "CL bootnode ENR path: $L2_CL_BOOTNODE_ENR_PATH"
  echo "Sequencer-1 P2P key written to $output_dir/sequencer-1-p2p-key.txt"
  echo "Sequencer-2 P2P key written to $output_dir/sequencer-2-p2p-key.txt"
}
