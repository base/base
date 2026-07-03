# Integrando Safe-Core Governance com dmux

## Visão Geral

O [dmux](https://github.com/standardagents/dmux) é um multiplexador para agentes de codificação que isola cada tarefa em um git worktree separado. O Safe-Core Governance adiciona **governança por worktree** — cada agente tem seu próprio estado ético, persistente e auditável.

## Arquitetura

```
┌─────────────────────────────────────────────────────────────────┐
│                         dmux (CLI + tmux)                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐│
│  │  Worktree 1 │  │  Worktree 2 │  │  Worktree N             ││
│  │  (Agente A) │  │  (Agente B) │  │  (Agente N)             ││
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────────────────┘│
│         │                │                │                    │
│         └────────────────┼────────────────┘                    │
│                          │ MCP (stdio)                        │
│                          ▼                                     │
│              ┌───────────────────────┐                        │
│              │ Safe-Core Governance  │                        │
│              │  MCP Server           │                        │
│              │  (worktree-scoped)    │                        │
│              └───────────────────────┘                        │
└─────────────────────────────────────────────────────────────────┘
```

## Integração Passo a Passo

### 1. Configure o Safe-Core para Isolamento por Worktree

Cada worktree do dmux deve ter seu próprio estado de governança:

```bash
# ~/.dmux/hooks/on-worktree-create.sh
#!/bin/bash
WORKTREE_PATH="$1"
WORKTREE_NAME="$2"

# Cria banco de dados dedicado para este worktree
mkdir -p "$WORKTREE_PATH/.safe-core"
export SAFE_CORE_DB="$WORKTREE_PATH/.safe-core/state.db"

# Inicia servidor MCP para este worktree (em background)
safe-core-governance-mcp --db "$SAFE_CORE_DB" --socket "$WORKTREE_PATH/.safe-core/mcp.sock" &
```

### 2. Hook de Pré-Merge — Verificação Ética

O dmux suporta hooks de ciclo de vida. Adicione validação ética antes de cada merge:

```bash
# ~/.dmux/hooks/pre-merge.sh
#!/bin/bash
WORKTREE_PATH="$1"
BRANCH_NAME="$2"

# Chama o Safe-Core para verificar o merge
RESULT=$(safe-core-mcp-client call enforce_action \
    --action "merge_branch" \
    --context "{\"branch\": \"$BRANCH_NAME\", \"worktree\": \"$WORKTREE_PATH\"}")

if echo "$RESULT" | grep -q '"verdict":"Block"'; then
    echo "❌ Merge bloqueado pela governança ética"
    echo "Razão: $(echo "$RESULT" | jq -r '.reason')"
    exit 1
fi

echo "✅ Merge aprovado pela governança"
```

### 3. Comandos Customizados no dmux

Adicione comandos customizados ao dmux para interagir com o Safe-Core:

```javascript
// ~/.dmux/plugins/safe-core.js
module.exports = {
    name: 'safe-core',
    commands: {
        'govern:status': async (args) => {
            // Mostra o estado da governança para o worktree atual
            const ws = getCurrentWorktree();
            const result = await callMCP('audit_root', { worktree: ws });
            console.log(`🔒 Raiz Merkle: ${result.root}`);
            console.log(`📊 Regras ativas: ${result.rule_count}`);
        },
        'govern:rule:add': async (args) => {
            const { action, constraint, severity } = args;
            await callMCP('create_rule', { action, constraint, severity });
            console.log(`✅ Regra criada: ${action} → ${constraint}`);
        },
        'govern:audit': async (args) => {
            const events = await callMCP('audit_events', { limit: 20 });
            console.table(events);
        }
    }
};
```

### 4. Integração com o Merge Menu

O dmux tem um menu `m` que oferece Merge e Create PR. Integre a verificação ética:

```javascript
// ~/.dmux/plugins/safe-core-merge.js
// Substitui o handler de merge padrão

dmux.on('mergeRequested', async ({ worktree, branch }) => {
    // 1. Verifica se o merge é eticamente permitido
    const verdict = await enforceAction('merge_branch', {
        branch,
        worktree: worktree.path,
        commit_count: await getCommitCount(branch)
    });

    if (verdict.verdict === 'Block') {
        dmux.notify(`❌ Merge bloqueado: ${verdict.reason}`, { type: 'error' });
        return;
    }

    if (verdict.verdict === 'RequireApproval') {
        dmux.notify(`⚠️ Merge requer aprovação humana: ${verdict.reason}`, { type: 'warning' });
        // Abre diálogo de aprovação
        const approved = await dmux.prompt('Aprovar merge?', { buttons: ['Sim', 'Não'] });
        if (!approved) return;
    }

    // 2. Executa o merge
    await dmux.doMerge(worktree, branch);

    // 3. Registra no audit trail
    await callMCP('audit_log', {
        action: 'merge_completed',
        branch,
        worktree: worktree.path,
        verdict: 'Allow'
    });

    dmux.notify('✅ Merge concluído com sucesso', { type: 'success' });
});
```

## Benefícios

| Funcionalidade | dmux nativo | + Safe-Core Governance |
|----------------|-------------|------------------------|
| Isolamento | ✅ Worktrees | ✅ + governança por worktree |
| Merge | ✅ Auto-merge | ✅ + verificação ética |
| Branches | ✅ Geradas por IA | ✅ + validação de regras |
| Auditoria | ❌ Não possui | ✅ Trilha Merkle |
| Persistência | ❌ Estado volátil | ✅ Regras imortais |

## Exemplo de Uso

```bash
# 1. No dmux, crie um novo pane para uma tarefa
dmux n
# Prompt: "Implementar feature de aumento de preço"

# 2. O agente tenta implementar aumento de 25%
# 3. Safe-Core bloqueia (regra: percentage <= 20)
# 4. dmux mostra notificação no painel
# 5. O agente ajusta para 15% e o merge é aprovado
# 6. Auditoria registra ambos os eventos
```