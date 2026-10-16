# Cloud Sync — tarefas

| ID | Estado | Evidência / próximo passo |
|---|---|---|
| DISC-01/02 | Concluído | Persistência, recuperação, dirty state e assets mapeados em CODEMAP.md |
| ARCH-01 | Concluído | WebDAV pessoal; cliente compartilhado; ADR em cloud-sync/ARCHITECTURE.md |
| IMPL-01/02/03 | Implementado | Checkpoints duráveis, retry, revisões imutáveis, conflitos e cofre de credenciais |
| IMPL-04 | Concluído | Janela de projetos e estados nos dois editores; capturas inspecionadas |
| QA-01 | Concluído | Photo 9 testes/Vector 10; ambos iniciados, checkpoints lidos e projetos reabertos após reinício |
| QA-02 | Concluído no cliente | Reinício offline, idempotência, status e dois clientes com rclone real |
| QA-03 | Parcial | Duas instâncias locais por HTTPS aprovadas; faltam VPS e dois computadores reais |
| DOC-01 | Concluído | STATE/CODEMAP/TASKS/TESTS atualizados; script inteiro reexecutado com exit 0; instalação/start salvos em rascunho, publicação pendente |
| DEPLOY-01 | Aguardando infraestrutura | Domínio/DNS, acesso e revisão da configuração da Contabo; nada implantado |
| HOME-01 | TODO | Tela inicial estilo Figma para PhotoCraft e VectorCraft, com todos os projetos/posts, prévias e abertura pela nuvem; escopo abaixo |

## HOME-01 — tela inicial de projetos na nuvem

Solicitado pelo proprietário em 2026-10-15. Pendente de implementação; não faz parte do release Windows atual.

- [ ] Abrir o aplicativo em uma biblioteca estilo Figma com todos os projetos/posts da conta, cards com miniaturas/prévias, nome, última edição e estado da sincronização.
- [ ] Permitir buscar, ordenar e filtrar projetos, acessar recentes e criar, renomear e abrir documentos diretamente pela biblioteca.
- [ ] Salvar alterações automaticamente na nuvem, sem exigir escolher uma pasta ou criar arquivos manuais no PC; exportar ou salvar uma cópia local somente por ação explícita.
- [ ] Carregar documentos sob demanda e manter somente o cache local necessário para desempenho, recuperação e edição offline, com limite de espaço e limpeza segura de cópias já sincronizadas.
- [ ] Nunca remover edições pendentes de envio, checkpoints necessários à recuperação ou cópias de conflito durante a limpeza do cache.
- [ ] Exibir estados de carregamento, biblioteca vazia, offline e falha; preservar a edição offline e retomar o envio ao reconectar.
- [ ] Validar nos dois aplicativos que um projeto criado, editado e reaberto pela tela inicial mantém o conteúdo e a prévia sem criar arquivos soltos em Documentos/Desktop; verificar também recuperação offline e limite do cache.
