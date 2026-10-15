# Cloud Sync — evidências

Validação no ambiente Linux gerenciado, Rust 1.95.0, em 2026-10-15. Os dois checkouts irmãos são necessários atualmente.

| Verificação | Resultado |
|---|---|
| PhotoCraft CLI, `cargo test --locked -p photocraft-cli --test cli` | 29 aprovados |
| VectorCraft CLI, `cargo test --locked -p vectorcraft-cli --test cli` | 8 aprovados |
| Cliente compartilhado, `cargo test --locked` em `cloud-sync/` | 12 aprovados; 1 teste de servidor externo ignorado na execução normal |
| Cliente, Clippy com `--all-targets -- -D warnings` | Aprovado |
| Duas instâncias com rclone real | Upload/download de snapshots sintéticos, dois ramos concorrentes e retomada após reinício offline aprovados |
| Caddy/rclone por HTTPS | Cliente real, autenticação, PROPFIND 207, MOVE 201 e recusa de sobrescrita 412 aprovados; confiança TLS verificada |
| Configuração de implantação | Compose, Caddy, sintaxe shell e healthchecks aprovados com recursos descartáveis; nenhum acesso à VPS |
| PhotoCraft, testes do adaptador nativo | 9 aprovados; inclui regressão de abertura em sessão vazia |
| VectorCraft, testes do adaptador nativo | 10 aprovados |
| VectorCraft UI, `cargo check --target wasm32-unknown-unknown -p vectorcraft-ui-egui` | Aprovado; adaptador é exclusivo do desktop |
| PhotoCraft, Clippy do pacote com `--all-targets --no-deps -- -D warnings` | Aprovado na fonte final, incluindo correção da reabertura |
| VectorCraft UI, Clippy do pacote com `--no-deps -- -D warnings` | Aprovado |
| Clippy amplo VectorCraft | Bloqueado por `collapsible_match` preexistente em `crates/pdf/src/import_scan.rs:549`; não alterado |
| PhotoCraft nativo no Xvfb | Criação 40×30, camada/pincelada, checkpoint automático `.pcraft`, inspeção CLI e reabertura após reinício aprovados; captura de tela inspecionada |
| VectorCraft nativo no Xvfb | Criação 300×200 com retângulo, checkpoint `.vectorcraft`, inspeção CLI e reabertura após reinício aprovados; nova edição gerou checkpoint com dois retângulos editáveis, sem avisos |

Os testes do cliente verificam fila durável, idempotência após reinício, integridade, isolamento de conta, grafo de versões, preservação de conflitos e classificação de falhas. Os testes dos adaptadores cobrem as fronteiras entre checkpoint local e fila, recuperação e restrições de acesso a arquivos vinculados.

O teste rclone externo é opt-in porque exige serviço descartável autenticado; não executar fixtures em armazenamento pessoal. Os serviços e credenciais de teste foram removidos. Credenciais nunca fazem parte da configuração salva.

Não executado: implantação Contabo, teste em dois computadores físicos, Windows/macOS e cofre nativo nesses sistemas. A suíte local não substitui esses testes de aceitação. O limite de snapshot é 256 MiB e não há coleta automática de histórico.

A execução real encontrou e corrigiu a abertura PhotoCraft em sessão vazia: o adaptador agora chama `sync_views` ao adicionar o documento, antes do próximo desenho. O nono teste cobre esse ciclo. O setup também passou a extrair o runtime DBus e desabilitar MIT-SHM no Xvfb, necessários neste contêiner. Captura PhotoCraft: `/workspace/cloud-setup/photocraft-cloud-reopened.png`.

O teste de transporte usa payloads sintéticos e compara seus bytes; a leitura e editabilidade dos formatos reais são verificadas separadamente nos adaptadores e no smoke nativo. Não se afirma um teste completo entre dois desktops pela VPS.

Captura VectorCraft após reabertura: `/workspace/cloud-setup/vectorcraft-cloud-reopened.png`, inspecionada. O checkpoint de continuação tem sequência 2, uma camada e dois objetos Rectangle, sem avisos no leitor nativo. Ambos os executáveis desktop foram compilados com `--locked` e iniciados com renderização de software no Xvfb; isso não valida aceleração por GPU física.

Reexecução integral de `bash /workspace/cloud-setup/install.sh`: exit 0. Rust/toolchain e bibliotecas preparadas novamente, builds CLI, 29 + 8 testes de integração, checks desktop dos dois apps e 12 testes do cliente aprovados. O teste externo continuou explicitamente ignorado nessa execução, pois não havia servidor descartável ativo. O script e as instruções de inicialização completos foram salvos no rascunho de configuração; publicação e restauração em nova tarefa não foram executadas.

Antes da publicação na main: `cargo xtask layers` passou nos dois repositórios. `cargo xtask ci` do VectorCraft passou em fmt e parou no Clippy pela mesma falha preexistente `crates/pdf/src/import_scan.rs:549` (`collapsible_match`); seus estágios seguintes foram ignorados pelo runner. Os testes específicos e o smoke descritos acima foram executados separadamente.

Clippy amplo PhotoCraft (`cargo clippy --locked -p photocraft --all-targets -- -D warnings`) também parou em falha preexistente fora das alterações: `crates/raw/src/cr3.rs:145`, `collapsible_match`. O Clippy do pacote com `--no-deps` e o Clippy estrito do cliente compartilhado passaram conforme registrado acima.
