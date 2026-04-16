#!/bin/bash
set -euxo pipefail

: "${VSOCK_CID:?required}"

/lib/systemd/systemd-udevd --daemon

ALLOCATOR_CONFIG=/etc/nitro_enclaves/allocator.yaml
CPU_COUNT=$(grep cpu_count "$ALLOCATOR_CONFIG" | awk '{print $2}')
MEMORY_MIB=$(grep memory_mib "$ALLOCATOR_CONFIG" | awk '{print $2}')

./nitro-cli-config -i
./nitro-enclaves-allocator

./nitro-cli run-enclave --cpu-count "$CPU_COUNT" --memory "$MEMORY_MIB" --eif-path ./eif.bin --enclave-cid "$VSOCK_CID"

# nitro-cli run-enclave returns immediately; wait for enclave to reach RUNNING
TIMEOUT=120
ELAPSED=0
until ./nitro-cli describe-enclaves | grep -q '"State": "RUNNING"'; do
    sleep 2
    ELAPSED=$((ELAPSED + 2))
    if [ "$ELAPSED" -ge "$TIMEOUT" ]; then
        echo "Timed out waiting for enclave to reach RUNNING state"
        ./nitro-cli describe-enclaves
        exit 1
    fi
done

echo "Enclave is running"

ENCLAVE_ID=$(./nitro-cli describe-enclaves | grep -o '"EnclaveID": "[^"]*"' | head -1 | cut -d'"' -f4)
echo "Enclave ID: $ENCLAVE_ID"

# Poll until enclave exits
while true; do
    DESC=$(./nitro-cli describe-enclaves)
    if ! echo "$DESC" | grep -q '"State": "RUNNING"'; then
        echo "Enclave is no longer running"
        echo "$DESC"
        exit 1
    fi
    sleep 30
done
