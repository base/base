# Reth Node Troubleshooting Guide

Welcome! This guide helps you solve common issues when running a Reth node.

## 🚨 Common Issues & Solutions

### 1. Out of Memory (OOM) Errors

**Problem:** Node crashes with "memory limit exceeded" error

**Solution:**
- Increase system RAM (minimum 16GB, recommended 32GB+)
- Reduce cache settings in config:
  [cache]
max_block_cache = "1GB" # Instead of default 4GB
### 2. Slow Sync Speed

**Problem:** Node takes too long to sync with the network

**Solution:**
- Check your internet connection (need at least 10 Mbps)
- Use SSD storage (HDD is too slow)
- Increase peers: `--max-outbound-peers 100`

### 3. Peer Connection Issues

**Problem:** "Not enough peers" warning or can't connect

**Solution:**
- Check firewall settings (open port 30303)
- Use public bootnodes: `--bootnodes <bootstrap-node-enode>`
- Wait 5-10 minutes (nodes need time to find peers)

### 4. Database Corruption

**Problem:** "Invalid database" or sync fails repeatedly

**Solution:**
Backup your data
cp -r ~/.reth ~/.reth.backup

Delete corrupted database
rm -rf ~/.reth/db

Restart node (will resync from scratch)
reth node
### 5. High CPU Usage

**Problem:** CPU at 100% constantly

**Solution:**
- Reduce pruning frequency (less frequent = lower CPU)
- Lower concurrent requests: `--max-concurrent-requests 256`
- Run on more powerful hardware

## 📊 Performance Tips

- **SSD required** for fast sync (NVMe is best)
- **16GB RAM minimum**, 32GB recommended
- **Open ports** for peer connectivity
- **Stable internet** (no metered connections)

## 🔧 Debug Mode

Run with verbose logging:
RUST_LOG=debug reth node
This shows detailed information about what's happening.

## 💬 Need More Help?

- Check [Reth docs](https://docs.reth.rs/)
- Ask on [Discord](https://discord.gg/reth)
- Open an issue on [GitHub](https://github.com/base/node-reth)
