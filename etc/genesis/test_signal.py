#!/usr/bin/env python3
"""Exercise the upgrade CLI against real genesis contracts and the legacy mock.

Usage: python3 etc/genesis/test_signal.py /path/to/default-genesis
Requires Anvil, Cast, Forge, Cargo, and the prepared contracts bundle.
"""
import json
import os
import pathlib
import shutil
import shlex
import socket
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
GENESIS = pathlib.Path(sys.argv[1]).resolve()
KEY = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"


class UpgradeSignalTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.directory.cleanup)
        cls.work = pathlib.Path(cls.directory.name)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        cls.rpc = f"http://127.0.0.1:{port}"
        cls.anvil = subprocess.Popen(["anvil", "--host", "127.0.0.1", "--port", str(port),
                                     "--init", str(GENESIS / "el/genesis.json"), "--silent"],
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        cls.addClassCleanup(cls.stop_anvil)
        for _ in range(100):
            if subprocess.run(["cast", "block-number", "--rpc-url", cls.rpc], capture_output=True).returncode == 0:
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("Anvil did not start")

    @classmethod
    def stop_anvil(cls):
        cls.anvil.terminate()
        cls.anvil.wait(timeout=10)

    def run_cli(self, contract, *args):
        env_file = self.work / "signal.env"
        env_file.write_text(f"DEPLOYER_KEY={KEY}\nUPGRADE_SIGNAL_CONTRACT={contract}\n"
                            f"UPGRADE_SIGNAL_L1_RPC_URL={self.rpc}\nUPGRADE_SIGNAL_L2_RPC_URL={self.rpc}\n"
                            f"UPGRADE_SIGNAL_ENV_OUT={self.work / 'output.env'}\n")
        return subprocess.run(["bash", str(ROOT / "etc/scripts/devnet/upgrade-signal.sh"), *args],
                              env=dict(os.environ, UPGRADE_SIGNAL_ENV_FILES=str(env_file)),
                              text=True, capture_output=True)

    def schedule(self, contract):
        result = subprocess.check_output(["cast", "call", "--json", "--rpc-url", self.rpc,
                                          contract, "getSchedule()(uint64[])"], text=True)
        return list(map(int, json.loads(result)[0]))

    def test_real_contract_updates_and_preflight_failures(self):
        contract = json.loads((GENESIS / "l2/l1-addresses.json").read_text())["ProtocolVersionsProxy"]
        original = self.schedule(contract)
        result = self.run_cli(contract, "move-future", "azul", "--offset", "120")
        self.assertEqual(result.returncode, 0, result.stderr)
        scheduled = self.schedule(contract)
        self.assertEqual(scheduled[:10], original[:10])
        self.assertGreaterEqual(scheduled[10], original[0] + 3600 + 120)
        result = self.run_cli(contract, "set", "azul", str(original[0] + 60))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("notice", result.stderr)
        self.assertEqual(self.schedule(contract), scheduled)
        result = self.run_cli(contract, "set", "regolith", "0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("frozen", result.stderr)
        self.assertEqual(self.schedule(contract), scheduled)
        result = self.run_cli(contract, "set", "azul", "0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.schedule(contract), original)

    def test_anvil_origin_patch_preserves_generated_rollup(self):
        # Execute the actual bootstrap function against Anvil, without deploying
        # unrelated Nitro contracts or starting the proof workers.
        source = (ROOT / "etc/scripts/devnet/anvil-nitro-local.sh").read_text()
        function = source[source.index("load_rollup_config() {"):source.index("genesis_output_root() {")]
        output = self.work / "anvil-rollup"
        output.mkdir()
        generated = output / "generated.json"
        original = (GENESIS / "l2/rollup.json").read_bytes()
        generated.write_bytes(original)
        runtime = output / "rollup.json"
        variables = dict(GENERATED_ROLLUP_CONFIG=str(generated), ROLLUP_CONFIG=str(runtime),
                         STATE_DIR=str(output), L1_RPC=self.rpc,
                         L1_CHAIN_ID_VALUE=str(json.loads(original)["l1_chain_id"]))
        assignments = "\n".join(f"{key}={shlex.quote(value)}" for key, value in variables.items())
        subprocess.run(["bash", "-ec", assignments + "\n" + function + "\nload_rollup_config"], check=True)
        self.assertEqual(generated.read_bytes(), original)
        block = json.loads(subprocess.check_output(["cast", "rpc", "--rpc-url", self.rpc,
                                                    "eth_getBlockByNumber", "0x0", "false"], text=True))
        self.assertEqual(json.loads(runtime.read_text())["genesis"]["l1"]["hash"], block["hash"])

    def test_legacy_mock_still_uses_set_schedule(self):
        project = self.work / "mock"
        (project / "src").mkdir(parents=True)
        shutil.copy2(ROOT / "crates/utilities/test-utils/contracts/src/MockProtocolVersions.sol", project / "src")
        compiler = ROOT / "build/genesis/contracts/.tools/svm/0.8.25/solc-0.8.25"
        deployed = subprocess.check_output(["forge", "create", "--root", str(project), "--use", str(compiler),
                                            "--evm-version", "cancun", "--rpc-url", self.rpc,
                                            "--private-key", KEY, "--broadcast", "--json",
                                            "src/MockProtocolVersions.sol:MockProtocolVersions"], text=True)
        contract = json.loads(deployed)["deployedTo"]
        result = self.run_cli(contract, "set", "azul", "123")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.schedule(contract)[10], 123)


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
