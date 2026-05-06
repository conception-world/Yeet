# Fase 3 — UX da resolução de conflitos

Este documento descreve a experiência de usuário do fluxo de conflito ponta a
ponta: que evento dispara o painel, o que aparece na tela em cada momento, em
que ordem o usuário interage, e que armadilhas concretas observar no Studio.
Serve como guia tanto para QA manual quanto para futuras refatorações da UI
(`yeet-plugin/src/ui/ConflictResolver.luau`).

---

## 1. Quando o painel aparece

O painel só sobe quando o daemon emite `ConflictDetected`. Isso acontece
quando, depois do bootstrap, **as duas pontas (Studio e disco) divergiram da
`Tree_Base`** para o mesmo arquivo. Em pseudocódigo:

```
para cada arquivo modificado em qualquer ponta:
    se base == studio == fs        → noop
    se base == studio  ≠  fs       → push para studio (auto)
    se base ==   fs    ≠  studio   → push para disco  (auto)
    se base ≠ studio ≠ fs ≠ base  → CONFLITO → ConflictDetected
```

Tipos de conflito que o usuário verá no painel (todos definidos em
`ConflictResolver.luau` como `ConflictKindTag`):

| `kind`             | Quando ocorre                                                              |
|--------------------|----------------------------------------------------------------------------|
| `edit`             | Hunks textuais incompatíveis em ambas as pontas                            |
| `delete_vs_edit`   | Uma ponta apagou; a outra editou                                           |
| `create_vs_create` | Ambas criaram um arquivo com o mesmo path mas conteúdo diferente            |

Renomeação ainda **não** é detectada como tal; até a Fase 5 ela aparece como
`delete` em uma ponta + `create_vs_create` em outra. Mencionar no painel ainda
não — só não esperar magia.

---

## 2. Anatomia do widget

`ConflictResolver` cria seu próprio `DockWidgetPluginGui` separado do widget
principal:

- ID: `YeetConflicts`
- Estado inicial: float, 960×640, mín. 720×480
- Título: `Yeet — Conflicts`

Layout absoluto (sem `UIListLayout` no root — as posições foram cravadas para
evitar que o ScrollingFrame engula a preview):

```
┌───────────────────────────────────────────────────────────────────┐
│ Header  ◀  src/MyModule.luau (1/3)  ▶              edit-conflict  │  28 px
├───────────────────────────────────────────────────────────────────┤
│                                                                   │
│   ┌─ STUDIO ──────┬─ BASE ──────┬─ IDE ─────────┐                 │
│   │ -- changed in │ -- pristine │ -- changed by │                 │
│   │ -- studio     │             │ -- vscode      │                 │
│   ├─────────────────────────────────────────────┤  ← hunk row     │
│   │ [Studio] [IDE] [Both] [Edit Manually]       │                 │
│   └─────────────────────────────────────────────┘                 │
│                                                                   │
│   ...uma row por hunk em conflito...                              │
│                                                                   │
├───────────────────────────────────────────────────────────────────┤
│ Preview (read-only, atualiza a cada clique)                       │  180 px
│                                                                   │
├───────────────────────────────────────────────────────────────────┤
│ [Resolve all as Studio] [Resolve all as IDE]                      │
│ status: 2 of 4 hunks resolved                                     │  80 px
│ [Cancel]                                            [Apply]       │
└───────────────────────────────────────────────────────────────────┘
```

Cores são semânticas e devem ser memorizadas:

- **Azul** → Studio (a ponta do Roblox)
- **Cinza** → Base (snapshot do último estado sincronizado)
- **Laranja** → IDE / FS (a ponta do disco/VS Code/Antigravity)
- **Roxo** → escolha "Keep Both"
- **Verde-suave** → escolha "Edit Manually"

A borda (`UIStroke`) ao redor do bloco do hunk reflete a escolha atual; o
status no rodapé conta `n of total hunks resolved`.

---

## 3. Fluxo do usuário, passo a passo

### 3.1 Cenário base: edit-edit em um arquivo

1. Daemon detecta divergência e envia `ConflictDetected` com 1 file-view, 3
   hunks em conflito.
2. `Applier.dispatch` reconhece o tipo, chama o dispatcher injetado pelo
   `Widget` → `ConflictResolver:enqueue(conflicts)`.
3. O resolver:
   - Faz `gui.Enabled = true` (o widget aparece flutuante na tela);
   - Renderiza o header com `src/MyModule.luau (1/1)`;
   - Renderiza 3 hunk-rows; nenhum tem escolha ainda → status: `0 of 3
     resolved`, **Apply desabilitado**.
4. Usuário clica `Keep IDE` no hunk 1, `Keep Studio` no hunk 2, `Edit Manually`
   no hunk 3.
   - Após `Edit Manually`, surge um `TextBox` overlay pré-preenchido com o
     texto base; usuário digita o resultado desejado.
5. Cada clique recomputa a preview no rodapé via `computePreview(state)`.
6. Status passa para `3 of 3 resolved`, **Apply habilita**.
7. Usuário clica **Apply**.
   - `handlers.onResolve(resolution)` é chamado;
   - `Widget` envia `{type="conflict_resolved", resolutions=[{path,hunks}]}`
     pelo socket;
   - Resolver avança para o próximo arquivo da fila — neste caso a fila
     esvazia, `gui.Enabled = false`.
8. Daemon: aplica `rebuild_resolved` para gerar o conteúdo final, atualiza
   `tree_base` + `tree_studio` + `tree_fs`, faz broadcast de `FileChanged`
   para todos os clientes (incluindo o que mandou o resolve), e o `Applier`
   absorve o eco via `recentlyApplied`.

### 3.2 Cenário multi-arquivo

Se `ConflictDetected.conflicts` tem N entradas, `enqueue` empurra N
`FileState`s para `self.queue`. O usuário navega com as setas `◀ ▶` no header
(também atalhos: setas do teclado, se viéssemos a wirar — não há ainda).
**Apply** sempre se refere ao **arquivo atual**; ele NÃO faz commit em
batch — cada Apply emite um `conflict_resolved` separado por arquivo.

Razão da decisão: minimiza trabalho perdido se o socket cair no meio. O custo
é mais frames pequenos no fio, mas em conflito o volume é desprezível.

### 3.3 Cenário Cancel

`Cancel` apenas desliga o widget (`gui.Enabled = false`) e chama
`handlers.onCancel()`. **Não** envia nada ao daemon. O daemon mantém o arquivo
em `pending_conflicts`, então:

- `tree_studio[path]` e `tree_fs[path]` ficam congelados nos valores que
  causaram o conflito;
- Edições subsequentes em qualquer das pontas para esse path continuam sendo
  **bloqueadas** (`handle_*` chama `is_blocked(path)` antes de mexer);
- Para reabrir o painel, o usuário precisa fazer alguma operação que dispare
  um novo `ConflictDetected` — o atalho mais rápido é editar o arquivo na
  IDE outra vez.

> **Pitfall conhecido**: não há ainda um botão "Reabrir conflitos pendentes"
> no widget principal. Se o usuário clica Cancel sem perceber, ele fica
> achando que a sync travou. Adicionar `pending_conflicts.list()` + botão no
> widget principal é trabalho de Fase 4.

### 3.4 Cenário "Resolve all as X"

Atalhos no rodapé. "Resolve all as Studio" seta `choice = "keep_studio"` em
todos os hunks do **arquivo atual** e atualiza a preview. Não toca arquivos
fora do atual e não faz Apply automático — ainda exige clique em Apply.

---

## 4. Detalhes invisíveis que importam

### 4.1 Preview é uma aproximação

`computePreview` reconstrói o conteúdo final fazendo splice das escolhas em
cima do `base_content`. Isso é uma aproximação porque:

- Hunks **não conflitantes** (auto-merged silenciosamente pelo daemon) **não
  vêm na payload** de `ConflictDetected` por design (decisão arquitetural #1
  da fase). A preview portanto não os reflete.
- Texto final autoritativo é o que o daemon vai gerar via `rebuild_resolved`
  ao receber `conflict_resolved`.

Em termos de UX: a preview é fiel para os hunks que o usuário está vendo. Se
o usuário aplicar e o broadcast de `FileChanged` chegar de volta com
diferenças (auto-merges fora dos hunks), o `Applier` aplica e a janela do
script no Studio se atualiza — comportamento correto, pode surpreender.

### 4.2 Anti-eco bilateral

O resolver emite `conflict_resolved` mas **não** registra hash no
`recentlyApplied` antes (diferente do fluxo de `FileChanged` saindo do
`SourceWatcher`). Razão: o conteúdo final autoritativo é decidido pelo daemon
a partir das escolhas, não pelo plugin — o plugin não sabe o hash final ex
ante. O eco vem como `FileChanged` normal e o `Applier`:

1. Seta `recentlyApplied[path] = hash` antes de mutar `Source`;
2. Faz `inst.Source = content` dentro do `withRecording`;
3. O `SourceWatcher.GetPropertyChangedSignal` dispara, hashea, vê match em
   `isEcho`, retorna sem fazer nada.

Funciona — verificado nos testes do daemon — mas é um pormenor crítico para
quem for refatorar.

### 4.3 Ctrl+Z atômico

Cada Apply do daemon resulta em um único `FileChanged` por arquivo, e cada
`FileChanged` no plugin entra em um `withRecording("YeetFileChanged", ...)`.
Ou seja: um Ctrl+Z no Studio reverte a aplicação de **um arquivo**, não da
fila inteira. Se o usuário quiser desfazer 3 arquivos resolvidos, precisa de
3 Ctrl+Z.

### 4.4 Manual edit vê o texto de Base, não Studio nem IDE

`Edit Manually` pré-preenche o `TextBox` com `base_text` para que o usuário
parta do estado pristine e construa o resultado. Pré-preencher com Studio ou
IDE seria um viés implícito. Se o usuário quiser começar de uma das versões,
basta clicar no botão correspondente primeiro e depois `Edit Manually` —
isso preserva o `manualText` se já houver, mas a primeira entrada em manual
sempre parte do base.

> **Pitfall**: o `TextBox` atual aceita texto multilinha mas não tem syntax
> highlighting nem indent guides. Para arquivos longos, sugerir ao usuário
> editar manualmente fora do Studio (no editor da escolha dele) e depois
> colar — ou apenas usar Keep Both e limpar manualmente no Studio depois.
> Implementar um editor de código real (com `RichText` + tokenização) é
> deferido até a Fase 5.

---

## 5. Pitfalls de Studio que mordem

1. **Reload do plugin durante conflito aberto.** Se o usuário roda
   `sync-plugin.ps1` e dá Reload em Plugins enquanto o painel de conflito
   está aberto, o widget some, mas o daemon ainda tem `pending_conflicts`.
   Ao reconectar, o `Hello` atual manda `studio_snapshot=[]` (Fase 5 vai
   popular) e o conflito **pode ser perdido em silêncio**. Workaround: Apply
   ou Cancel antes de Reload.

2. **DockWidget flutuante esconde-se atrás do place.** Em monitores únicos,
   o widget pode aparecer atrás do viewport. Procurar por
   `gui.Enabled = true` no Output do plugin para confirmar que o evento
   chegou; se chegou mas nada visível, arrastar a barra "Yeet — Conflicts"
   visível na lista de plugins ancorados.

3. **TextBox de manual edit captura o `Source` do script.** Em alguns builds
   do Studio, focar em `TextBox` enquanto o script-editor está aberto
   intercepta atalhos como Ctrl+S. Sem consequência (o plugin não usa
   Ctrl+S), mas pode confundir.

4. **Race com edição contínua.** Enquanto o painel de conflito está aberto,
   o `SourceWatcher` continua ativo. Se o usuário edita o `Source` do mesmo
   arquivo no script-editor, isso vira mais um `FileChanged` saindo, e o
   daemon vai gerar OUTRO `ConflictDetected` (mesma resolução pendente +
   nova edição). Comportamento correto mas pode parecer "travado". Mitigação
   atual: nenhuma. Fase 4 deve considerar bloquear o `Source` enquanto há
   conflito pendente para o path.

---

## 6. Roteiro de validação manual

Sequência mínima para verificar que o fluxo funciona end-to-end:

1. Subir o daemon (`cargo run --release` na pasta `yeet-daemon`).
2. Buildar e instalar o plugin (`./scripts/sync-plugin.ps1` na raiz).
3. Abrir um place e clicar **Yeet → Open** → **Connect** no widget.
4. Confirmar bootstrap: scripts esperados aparecem no `DataModel`, log mostra
   `bootstrap: N file(s) materialized`.
5. Editar `src/MyModule.luau` no script-editor do Studio: linha 5, mudar
   `local x = 1` para `local x = 2`. Salvar (auto).
6. Sem fechar Studio, em outro editor, editar o mesmo arquivo no disco:
   linha 5, mudar para `local x = 3`. Salvar.
7. **Esperado**: dentro de 1–2 s, `ConflictDetected` chega no plugin, widget
   `Yeet — Conflicts` aparece, header mostra `src/MyModule.luau (1/1)`,
   1 hunk-row visível com painéis Studio (mostrando `local x = 2`), Base
   (`local x = 1`), IDE (`local x = 3`).
8. Clicar **Keep IDE** → preview mostra `local x = 3`. Clicar **Apply**.
9. **Esperado**: widget some; `Source` do script no Studio passa a
   `local x = 3`; arquivo no disco continua `local x = 3`; daemon loga
   `resolved 1 conflict`.
10. Ctrl+Z no Studio → `Source` volta ao `local x = 2` (estado pré-resolve
    do lado do Studio), e o `SourceWatcher` envia um novo `FileChanged` →
    novo `ConflictDetected` (Studio quer `2`, FS está em `3`, base agora é
    `3`). Comportamento intencional.

Se algum passo desviar, o ponto exato a inspecionar é o log do widget no
DockWidget principal — toda mensagem socket entra ali com prefixo `~`/`+`/`-`
ou `->`.
