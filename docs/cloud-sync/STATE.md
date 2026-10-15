# Cloud Sync — estado operacional

Data: 2026-10-15.

## Objetivo e decisão

Salvamento automático desktop, local primeiro, sincronização entre dispositivos, sem colaboração em tempo real. VPS Contabo informada pelo usuário: 12 GB RAM/200 GB. Serviço escolhido: rclone WebDAV atrás de Caddy/HTTPS, uma conta e raiz de armazenamento isoladas por pessoa. Sem servidor de autenticação próprio.

## Implementado

- Cliente Rust compartilhado com fila durável, reabertura offline, checksum, publicação imutável, identidade de projeto/versão e preservação de ramos concorrentes.
- Adaptadores nativos PhotoCraft/VectorCraft com captura em segundo plano, janela Cloud Projects, estados discretos e checkpoint local independente do salvamento manual/recuperação.
- Configuração de implantação, preflight e backup em `cloud-sync/deploy/`.

## Validação confirmada

- PhotoCraft CLI: 29 testes de integração passaram.
- VectorCraft CLI: 8 testes de integração passaram.
- Cliente: 12 testes passaram; teste rclone externo explicitamente ignorado na suíte normal e executado separadamente com sucesso.
- Rclone real: duas instâncias trocaram payloads sintéticos, mantiveram dois ramos concorrentes e retomaram fila após reinício offline.
- HTTPS/Caddy+rclone: autenticação, PROPFIND e MOVE com proteção de sobrescrita passaram.

Testes dos adaptadores: PhotoCraft 9 aprovados e VectorCraft 10 aprovados; Clippy dos pacotes alterados aprovado. PhotoCraft executado no Xvfb: criação, edição, checkpoint automático e reabertura após reinício aprovados. VectorCraft também compilado e executado: projeto 300×200, checkpoint, reabertura e continuação da edição aprovados; leitor nativo confirmou uma camada e dois retângulos sem avisos. Capturas de tela dos dois aplicativos inspecionadas.

## Limites e pré-requisitos

- VPS acessada em 2026-10-15, fingerprint ED25519 conferido. Backend rclone implantado e saudável em `~/artcraft-cloud-sync`, na rede interna `artcraft_cloud_dav`, sem porta publicada. A conta dedicada foi criada com hash bcrypt e a credencial operacional foi salva no cofre seguro local. A rota `cloudsync.madebyfelipe.agency` foi validada e adicionada ao Caddy existente por reload, sem recriação. O proprietário criou o registro A; certificado público Let’s Encrypt emitido e preflight HTTPS original aprovado. Backup externo permanece pendente.
- Nenhum teste em dois computadores físicos foi executado.
- Nesta sessão local, a suíte compartilhada passou novamente (12 testes); o teste externo existente também passou contra a VPS por túnel SSH privado, com dois caches isolados, dois ramos concorrentes preservados e retomada após reinício offline (54,92 s). A coleção descartável foi removida. Isso ainda não valida o endereço HTTPS público nem documentos nativos entre desktops.
- Servidor e usuário são persistidos sem senha. Quando disponível, o cofre seguro do sistema guarda a credencial e permite reconexão automática; sem ele, a credencial vale somente para a sessão.
- Para compilar, VectorCraft depende provisoriamente do checkout irmão PhotoCraft para o cliente compartilhado; os executáveis distribuídos não exigem esses checkouts.
- Snapshots limitados a 256 MiB; histórico e uploads temporários interrompidos não têm coleta automática. Acompanhar armazenamento e fazer backup.
- Conflitos são cópias preservadas; não há fusão automática.
- Fontes continuam seguindo o formato nativo existente; não foi criado serviço de distribuição de fontes.
- Originais vinculados faltantes, opacos ou fora da pasta autorizada do projeto exigem incorporação/atualização no editor; o checkpoint local permanece preservado, mas a versão incompleta não é enviada.
- PhotoCraft mantém automação da janela Cloud Projects pelos métodos autenticados existentes de clique/digitação; não foi criado desvio dos controles de expiração e acesso para uma API de nuvem paralela.

## Ambiente reutilizável

`/workspace/cloud-setup/install.sh` foi reexecutado integralmente com exit 0; inicialização gráfica e fluxos nativos foram exercitados. `install_script` e `start_skill` completos salvos no rascunho do ambiente. O usuário ainda precisa revisar/salvar nas configurações e publicar. Nenhuma restauração em nova tarefa foi verificada. Na preparação original, a implantação na VPS não foi realizada; ela foi iniciada nesta sessão local conforme o estado operacional acima. O guia de execução está em `cloud-sync/README.md`.

## Escopo final desta sessão local

Por solicitação do proprietário para economizar o limite, os testes restantes foram interrompidos para priorizar os executáveis release dos dois aplicativos. Os 9 testes do adaptador PhotoCraft passaram novamente; o build de desenvolvimento e a criação de um checkpoint nativo local foram executados. Recuperação/reconexão do desktop e os testes VectorCraft desta sessão não foram concluídos. Endpoint/usuário dos dois perfis locais padrão foram configurados, com senha no cofre seguro. Os builds release estão em execução em `../artcraft-cloud-ops/`, com LTO desativado para acelerar a compilação.

HTTPS público concluído: `https://cloudsync.madebyfelipe.agency/`; preflight original aprovado com TLS verificado, PROPFIND, PUT, MOVE, recusa de sobrescrita 412 e ambos os conteúdos preservados.
