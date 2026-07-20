import init, { WasmDevnet } from './pkg/base_wasm_devnet.js';

let devnet = null;
let isPlaying = false;
let loopTimer = null;
let currentSpeed = 1.0;
let isTicking = false;

const els = {
    statusIndicator: document.getElementById('status-indicator'),
    statusText: document.getElementById('status-text'),
    btnTogglePlay: document.getElementById('btn-toggle-play'),
    speedSlider: document.getElementById('speed-slider'),
    speedDisplay: document.getElementById('speed-display'),
    
    statChainId: document.getElementById('stat-chain-id'),
    statL1Tip: document.getElementById('stat-l1-tip'),
    statSeqHead: document.getElementById('stat-seq-head'),
    statSafeHead: document.getElementById('stat-safe-head'),
    statVerified: document.getElementById('stat-verified'),
    statNonce: document.getElementById('stat-nonce'),
    statBalance: document.getElementById('stat-balance'),
    
    statPendingBlocks: document.getElementById('stat-pending-blocks'),
    statTotalFrames: document.getElementById('stat-total-frames'),
    statCompressionRatio: document.getElementById('stat-compression-ratio'),
    statLastBatch: document.getElementById('stat-last-batch'),
    
    txSender: document.getElementById('tx-sender'),
    txRecipient: document.getElementById('tx-recipient'),
    txAmount: document.getElementById('tx-amount'),
    btnSendTx: document.getElementById('btn-send-tx'),
    btnPresetSelf: document.getElementById('btn-preset-self'),
    txStatusContainer: document.getElementById('tx-status-container'),
    txStatusLog: document.getElementById('tx-status-log'),
    
    blocksFeed: document.getElementById('blocks-feed'),
    
    btnToggleRpc: document.getElementById('btn-toggle-rpc'),
    rpcLog: document.getElementById('rpc-log'),
    
    btnToggleAbout: document.getElementById('btn-toggle-about'),
    aboutContent: document.getElementById('about-content'),
    
    consoleMethod: document.getElementById('rpc-console-method'),
    consoleParams: document.getElementById('rpc-console-params'),
    btnConsoleSend: document.getElementById('btn-rpc-console-send'),
    consoleResponse: document.getElementById('rpc-console-response')
};

let rpcId = 1;
function rpc(method, params = []) {
    const req = {
        jsonrpc: "2.0",
        id: rpcId++,
        method,
        params
    };
    const reqStr = JSON.stringify(req);
    const resStr = devnet.rpc_request(reqStr);
    const res = JSON.parse(resStr);
    
    logRpc(req, res);
    
    if (res.error) {
        throw new Error(res.error.message);
    }
    return res.result;
}

function logRpc(req, res) {
    const entry = document.createElement('div');
    entry.className = 'rpc-entry';
    
    const reqEl = document.createElement('div');
    reqEl.className = 'rpc-req';
    reqEl.innerHTML = `> <span class="rpc-method">${req.method}</span> ${JSON.stringify(req.params)}`;
    
    const resEl = document.createElement('div');
    resEl.className = 'rpc-res';
    if (res.error) {
        resEl.style.color = 'var(--accent-danger)';
        resEl.textContent = `< error: ${res.error.message}`;
    } else {
        let resText = JSON.stringify(res.result);
        if (resText.length > 100) resText = resText.substring(0, 100) + '...';
        resEl.textContent = `< ${resText}`;
    }
    
    entry.appendChild(reqEl);
    entry.appendChild(resEl);
    
    els.rpcLog.prepend(entry);
    
    while (els.rpcLog.children.length > 50) {
        els.rpcLog.removeChild(els.rpcLog.lastChild);
    }
}

function hexToBytes(hex) {
    if (hex.startsWith('0x')) hex = hex.slice(2);
    if (hex.length % 2 !== 0) throw new Error('Invalid hex string');
    const bytes = new Uint8Array(hex.length / 2);
    for (let i = 0; i < hex.length; i += 2) {
        bytes[i / 2] = parseInt(hex.slice(i, i + 2), 16);
    }
    return bytes;
}

function bytesToHex(bytes) {
    return '0x' + Array.from(bytes).map(b => b.toString(16).padStart(2, '0')).join('');
}

function ethToWei(ethStr) {
    const parts = ethStr.split('.');
    let whole = parts[0] || '0';
    let fraction = parts[1] || '';
    if (fraction.length > 18) fraction = fraction.slice(0, 18);
    while (fraction.length < 18) fraction += '0';
    return BigInt(whole + fraction);
}

function weiToEth(weiBigInt) {
    const weiStr = weiBigInt.toString().padStart(19, '0');
    const whole = weiStr.slice(0, -18) || '0';
    const fraction = weiStr.slice(-18).replace(/0+$/, '');
    return fraction ? `${whole}.${fraction}` : whole;
}

let totalFramesSubmitted = 0;
let pendingBlocks = 0;

async function tick() {
    if (isTicking) return;
    isTicking = true;
    try {
        await devnet.run_epoch(0n, 1n);
        
        pendingBlocks++;
        const frames = Number(devnet.last_submitted_frame_count());
        if (frames > 0) {
            totalFramesSubmitted += frames;
            const ratio = (pendingBlocks / frames).toFixed(2);
            els.statCompressionRatio.textContent = `${ratio}x`;
            els.statLastBatch.textContent = `[ OK ] ${pendingBlocks} BLOCKS → ${frames} FRAMES`;
            els.statLastBatch.style.color = 'var(--accent-primary)';
            pendingBlocks = 0;
        }
        
        if (els.statPendingBlocks) els.statPendingBlocks.textContent = pendingBlocks;
        if (els.statTotalFrames) els.statTotalFrames.textContent = totalFramesSubmitted;

        await updateStats();
        await updateBlocks();
        checkPendingTxs();
    } catch (e) {
        console.error(e);
    } finally {
        isTicking = false;
        scheduleNextTick();
    }
}

function scheduleNextTick() {
    if (loopTimer) clearTimeout(loopTimer);
    if (isPlaying) {
        const interval = 2000 / currentSpeed;
        loopTimer = setTimeout(tick, interval);
    }
}

function togglePlay() {
    isPlaying = !isPlaying;
    els.btnTogglePlay.textContent = isPlaying ? '⏸ PAUSE' : '▶ PLAY';
    els.statusIndicator.style.animation = isPlaying ? 'pulse 2s infinite' : 'none';
    if (isPlaying) {
        tick();
    } else {
        if (loopTimer) clearTimeout(loopTimer);
    }
}

let devAddress = '';

async function updateStats() {
    els.statChainId.textContent = Number(devnet.chain_id());
    els.statL1Tip.textContent = Number(devnet.l1_tip_number());
    els.statSeqHead.textContent = Number(devnet.sequencer_head_number());
    els.statSafeHead.textContent = Number(devnet.validator_safe_number());
    els.statVerified.textContent = Number(devnet.verified_block_count());
    
    try {
        const balanceHex = rpc('eth_getBalance', [devAddress, 'latest']);
        const balanceWei = BigInt(balanceHex);
        els.statBalance.textContent = weiToEth(balanceWei);
        
        const nonceHex = rpc('eth_getTransactionCount', [devAddress, 'latest']);
        els.statNonce.textContent = parseInt(nonceHex, 16).toString();
    } catch (e) {
        console.error(e);
    }
}

let lastRenderedBlock = -1;

async function updateBlocks() {
    const seqHead = Number(devnet.sequencer_head_number());
    const safeHead = Number(devnet.validator_safe_number());
    
    const startBlock = Math.max(0, seqHead - 20, lastRenderedBlock + 1);
    
    for (let i = startBlock; i <= seqHead; i++) {
        const blockHex = '0x' + i.toString(16);
        const block = rpc('eth_getBlockByNumber', [blockHex, false]);
        if (!block) continue;
        
        const row = document.createElement('div');
        row.className = 'block-row';
        row.id = `block-row-${i}`;
        
        const shortHash = block.hash.substring(0, 10) + '...';
        
        row.innerHTML = `
            <div class="block-num">${i}</div>
            <div class="block-hash" title="${block.hash}">${shortHash}</div>
            <div class="block-txs">${block.transactions.length}</div>
            <div class="block-ver" id="block-ver-${i}">-</div>
        `;
        
        els.blocksFeed.prepend(row);
        
        while (els.blocksFeed.children.length > 20) {
            els.blocksFeed.removeChild(els.blocksFeed.lastChild);
        }
        
        lastRenderedBlock = i;
    }
    
    for (let i = Math.max(0, seqHead - 20); i <= safeHead; i++) {
        const verCell = document.getElementById(`block-ver-${i}`);
        if (verCell && verCell.textContent === '-') {
            verCell.textContent = '✓';
            verCell.style.color = 'var(--accent-primary)';
        }
    }
}

const pendingTxs = new Set();

function checkPendingTxs() {
    for (const txHash of pendingTxs) {
        const receipt = rpc('eth_getTransactionReceipt', [txHash]);
        if (receipt) {
            pendingTxs.delete(txHash);
            logTxStatus(txHash, `Confirmed in Block #${parseInt(receipt.blockNumber, 16)}`, 'log-success');
        }
    }
}

function logTxStatus(hash, message, className) {
    els.txStatusContainer.classList.remove('hidden');
    const entry = document.createElement('div');
    entry.className = `log-entry ${className}`;
    const shortHash = hash.substring(0, 10) + '...';
    entry.textContent = `[${shortHash}] ${message}`;
    els.txStatusLog.prepend(entry);
}

async function sendTx() {
    try {
        els.btnSendTx.disabled = true;
        const toHex = els.txRecipient.value.trim();
        if (toHex.length !== 42) throw new Error('Recipient must be 42 characters (0x + 40 hex)');
        const toBytes = hexToBytes(toHex);
        
        const ethAmount = els.txAmount.value.trim();
        const weiAmount = ethToWei(ethAmount);
        
        const rawBytes = devnet.create_test_transfer(toBytes, weiAmount);
        const rawHex = bytesToHex(rawBytes);
        
        const txHash = rpc('eth_sendRawTransaction', [rawHex]);
        
        pendingTxs.add(txHash);
        logTxStatus(txHash, 'Pending in mempool...', 'log-pending');
        
    } catch (e) {
        logTxStatus('ERROR', e.message, 'log-error');
    } finally {
        els.btnSendTx.disabled = false;
    }
}

function sendConsoleRpc() {
    const method = els.consoleMethod.value.trim();
    if (!method) {
        els.consoleResponse.textContent = '// error: method is required';
        return;
    }

    let params;
    try {
        const raw = els.consoleParams.value.trim() || '[]';
        params = JSON.parse(raw);
        if (!Array.isArray(params)) throw new Error('params must be a JSON array');
    } catch (e) {
        els.consoleResponse.textContent = `// invalid params JSON: ${e.message}`;
        return;
    }

    els.btnConsoleSend.disabled = true;
    try {
        const req = { jsonrpc: '2.0', id: rpcId++, method, params };
        const reqStr = JSON.stringify(req);
        const resStr = devnet.rpc_request(reqStr);
        const res = JSON.parse(resStr);
        logRpc(req, res);
        els.consoleResponse.textContent = JSON.stringify(res, null, 2);
    } catch (e) {
        els.consoleResponse.textContent = `// exception: ${e.message}`;
    } finally {
        els.btnConsoleSend.disabled = false;
    }
}

async function startApp() {
    try {
        await init();
        devnet = await WasmDevnet.create();
        
        devAddress = devnet.dev_account_address();
        els.txSender.value = devAddress;
        
        els.statusText.textContent = 'ONLINE / READY';
        els.statusIndicator.classList.remove('offline');
        
        els.btnTogglePlay.disabled = false;
        els.speedSlider.disabled = false;
        els.btnSendTx.disabled = false;
        
        els.btnTogglePlay.addEventListener('click', togglePlay);
        els.speedSlider.addEventListener('input', (e) => {
            currentSpeed = parseFloat(e.target.value);
            els.speedDisplay.textContent = currentSpeed.toFixed(1) + 'x';
            if (isPlaying && loopTimer) {
                clearTimeout(loopTimer);
                scheduleNextTick();
            }
        });
        
        els.btnPresetSelf.addEventListener('click', () => {
            els.txRecipient.value = devAddress;
        });
        
        els.btnSendTx.addEventListener('click', sendTx);
        
        els.btnConsoleSend.disabled = false;
        els.btnConsoleSend.addEventListener('click', sendConsoleRpc);
        
        let rpcCollapsed = false;
        els.btnToggleRpc.addEventListener('click', () => {
            rpcCollapsed = !rpcCollapsed;
            if (rpcCollapsed) {
                els.rpcLog.classList.add('collapsed');
                els.btnToggleRpc.textContent = 'EXPAND';
            } else {
                els.rpcLog.classList.remove('collapsed');
                els.btnToggleRpc.textContent = 'COLLAPSE';
            }
        });
        
        let aboutCollapsed = false;
        els.btnToggleAbout.addEventListener('click', () => {
            aboutCollapsed = !aboutCollapsed;
            if (aboutCollapsed) {
                els.aboutContent.classList.add('collapsed');
                els.btnToggleAbout.textContent = 'EXPAND';
            } else {
                els.aboutContent.classList.remove('collapsed');
                els.btnToggleAbout.textContent = 'COLLAPSE';
            }
        });
        
        await updateStats();
        await updateBlocks();
        
        togglePlay();
        
    } catch (e) {
        els.statusText.textContent = 'FAILED TO INITIALIZE';
        els.statusText.style.color = 'var(--accent-danger)';
        els.statusIndicator.classList.add('offline');
    }
}

startApp();
