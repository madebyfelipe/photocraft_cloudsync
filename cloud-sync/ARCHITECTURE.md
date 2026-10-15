# ADR: sincronização desktop por WebDAV pessoal

Decisão: PhotoCraft e VectorCraft usam cliente Rust compartilhado dentro dos
aplicativos nativos; rclone mantido pela comunidade serve WebDAV sobre filesystem
local do VPS, atrás de Caddy com HTTPS. Não há backend próprio nem autenticação
própria. Uma conta/raiz é exclusiva de uma pessoa; htpasswd não isola usuários.
Os arquivos de `deploy/` são uma implantação revisável, não uma implantação já
executada no VPS.

Cada documento usa identidade UUID estável; cada revisão ganha UUID novo, snapshot
nativo completo imutável e manifesto JSON com pais, tamanho e SHA-256. A raiz
compartilhada da mesma pessoa é plana: `<version-uuid>.payload` e
`<version-uuid>.json`; o manifesto discrimina Photo/Vector. Identidades UUID
evitam colisões entre documentos e aplicativos. O cliente
salva localmente antes de sincronizar, envia o snapshot por PUT temporário e MOVE
com `Overwrite: F`, verifica sucesso e publica o manifesto por último pelo mesmo
procedimento. Destinos existentes nunca são substituídos. O histórico é um DAG:
edições concorrentes preservam os dois ramos, sem mesclar binários nem escolher
silenciosamente o último upload. Nome visível não é identidade. Enumeração usa
PROPFIND; ausência em uma listagem não significa exclusão local.

A publicação depende da semântica MOVE do servidor: validar com o preflight antes
do uso e após upgrades. Manifesto publicado é a unidade visível de revisão;
objetos temporários ou snapshots órfãos não são revisões. Reinício, perda de rede
e disco cheio conservam cópia local e operações pendentes. O cliente valida
manifestos, caminhos, limites de tamanho e hash antes de instalar um arquivo
baixado; não confia em nomes ou metadados recebidos.

Consequências: servidor pequeno e operável, histórico recuperável e suporte
posterior a outros aplicativos Craft. Snapshots completos consomem disco/banda;
o histórico precisa de retenção explícita e backup externo consistente. HTTPS
protege transporte, não fornece criptografia ponta a ponta. A credencial DAV
permite exclusão; imutabilidade é contrato do cliente, não proteção contra um
cliente comprometido. Sem multiusuário sobre a mesma raiz, quota fictícia,
mesclagem automática de arquivos nativos ou promessa de sincronização instantânea.
