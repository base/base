# Integrando Safe-Core Governance com wmux

## Visão Geral

O [wmux](https://github.com/amirlehmam/wmux) é um multiplexador de terminal para Windows que oferece visibilidade em tempo real das sessões do Claude Code. O Safe-Core Governance adiciona uma **camada de consciência** — ética, memória, verdade e rastreabilidade — que o wmux pode consumir via MCP.

## Arquitetura

```
┌─────────────────────────────────────────────────────────────────┐
│                         wmux (UI)                              │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐│
│  │  Terminal   │  │  Browser    │  │  Sidebar (atividade)    ││
│  │  Panes      │  │  Panel      │  │  - Dots de status       ││
│  └─────────────┘  └─────────────┘  │  - Notificações         ││
│                                     └───────────┬─────────────┘│
└─────────────────────────────────────────────────┼────────────────┘
                                                  │ MCP (stdio/HTTP)
                                                  ▼
┌─────────────────────────────────────────────────────────────────┐
│              Safe-Core Governance MCP Server                    │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌─────────┐│
│  │  Ética      │  │ Persistência│  │ Verificação │  │Auditoria││
│  │(enforce)    │  │ (rules)     │  │ (verify)    │  │(merkle) ││
│  └─────────────┘  └─────────────┘  └─────────────┘  └─────────┘│
└─────────────────────────────────────────────────────────────────┘
```

## Integração Passo a Passo

### 1. Inicie o Servidor MCP do Safe-Core

```bash
# Clone e compile
git clone https://github.com/arkhe-research/safe-core
cd safe-core/crates/safe-core-governance
cargo build --release

# Inicie o servidor (stdio)
./target/release/safe-core-governance-mcp
```

### 2. Configure o wmux para consumir o MCP Server

O wmux suporta integração com servidores MCP via configuração. Adicione ao `~/.wmux/config.toml`:

```toml
[mcp]
enabled = true
servers = [
    {
        name = "safe-core-governance",
        command = "safe-core-governance-mcp",
        args = ["--stdio"],
        env = { SAFE_CORE_DB = "~/.safe-core/state.db" }
    }
]
```

### 3. Use as Ferramentas de Governança no wmux

No wmux, você pode chamar as ferramentas MCP diretamente:

```bash
# No terminal do wmux, usando o cliente MCP
echo '{"jsonrpc":"2.0","method":"tools/call","params":{"name":"enforce_action","arguments":{"action":"increase_price","context":{"percentage":15}}}}' | safe-core-mcp-client
```

### 4. Integração com o Plugin `wmux-orchestrator`

O wmux possui um plugin `wmux-orchestrator` que decompõe tarefas complexas em agentes paralelos. Você pode integrar o Safe-Core para governar cada agente:

```javascript
// wmux-orchestrator plugin — adicionar verificação ética
// ~/.wmux/plugins/safe-core-governance.js

const { spawn } = require('child_process');

async function enforce(action, context) {
    return new Promise((resolve, reject) => {
        const proc = spawn('safe-core-mcp-client', [
            'call', 'enforce_action',
            '--action', action,
            '--context', JSON.stringify(context)
        ]);
        proc.stdout.on('data', (data) => resolve(JSON.parse(data)));
        proc.stderr.on('data', (data) => reject(data));
    });
}

// Hook no lifecycle do orchestrator
orchestrator.on('beforeTask', async (task) => {
    const verdict = await enforce(task.action, task.context);
    if (verdict.verdict === 'Block') {
        throw new Error(`Tarefa bloqueada: ${verdict.reason}`);
    }
    return verdict;
});
```

## Benefícios

| Funcionalidade | wmux nativo | + Safe-Core Governance |
|----------------|-------------|------------------------|
| Visibilidade | ✅ Sidebar, dots | ✅ + razão do bloqueio |
| Notificações | ✅ OSC 9/99/777 | ✅ + alertas éticos |
| Persistência | ✅ Sessões salvas | ✅ Regras imortais |
| Auditoria | ❌ Não possui | ✅ Trilha Merkle |

## Exemplo de Uso

```bash
# 1. No wmux, crie um workspace com Claude Code
# 2. Claude Code tenta aumentar preço em 25%
# 3. Safe-Core bloqueia (regra: percentage <= 20)
# 4. wmux mostra notificação: "Ação bloqueada por governança ética"
# 5. Auditoria registra o evento com prova Merkle
```
