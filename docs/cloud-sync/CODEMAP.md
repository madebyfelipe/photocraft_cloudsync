# Cloud Sync — mapa

- `cloud-sync/src/lib.rs`: cliente compartilhado, fila durável, cache, WebDAV, revisões imutáveis e conflitos; testes no mesmo arquivo.
- `cloud-sync/src/credentials.rs`: cofre de credenciais do sistema, isolamento por conta e fallback de sessão.
- `apps/photocraft/src/cloud_sync.rs`: adaptador desktop, agendamento, captura nativa em segundo plano, fontes vinculadas, janela de projetos.
- `apps/photocraft/src/main.rs`: integração do adaptador com eframe.
- `../vectorcraft_cloudsync/crates/ui-egui/src/cloud_sync.rs`: adaptador VectorCraft; hooks em `lib.rs`, `chrome.rs` e `menus.rs`.
- `cloud-sync/deploy/`: configuração testada de rclone/Caddy para uma conta privada na VPS.
- `cloud-sync/ARCHITECTURE.md`: decisão WebDAV, revisões e limites.

Os formatos `.pcraft` e `.vectorcraft` existentes continuam sendo os documentos. O manifesto do sync contém somente identidade, ancestralidade e integridade da revisão. Recuperação de falhas e salvamento manual continuam independentes.
