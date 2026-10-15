# WebDAV pessoal no VPS

PhotoCraft e VectorCraft continuam aplicativos desktop nativos. Este serviço só
armazena snapshots e manifestos; não transforma o VPS em editor. Um VPS Contabo
com 12 GB de RAM e 200 GB de disco é suficiente para começar, mas a capacidade
real depende dos documentos, histórico, SO e backups. Nenhum acesso ao VPS é
necessário para revisar estes arquivos.

## Preparar e iniciar

Requisitos externos: Linux com Docker Engine e Compose v2, domínio próprio com
DNS A/AAAA apontando corretamente para o VPS, portas TCP 80/443 liberadas e livres,
saída HTTPS para a autoridade ACME. Remova AAAA incorreto. Se já houver proxy nas
portas, integre o domínio nele em vez de iniciar outro Caddy.

Execute a partir desta pasta, em uma cópia no VPS:

```sh
cp .env.example .env
# Edite DAV_DOMAIN em .env com o domínio real (sem https://).
mkdir -p data secrets
chmod 700 data secrets
# Instale apache2-utils pela distribuição; escolha seu nome de conta.
read -r -p 'Conta dedicada: ' DAV_USER
(umask 077; htpasswd -cB secrets/htpasswd "$DAV_USER")
# A senha é solicitada interativamente; não use -b nem a envie ao Git.
docker compose config --quiet
docker compose up -d
docker compose ps
```

O arquivo `htpasswd` contém hash bcrypt, fica montado somente para leitura e nunca
deve ser versionado. A pasta `data/` é persistente; `docker compose down` não a
remove. Os volumes Caddy preservam certificados e configuração. Não execute
`down -v` como rotina. As imagens estão fixadas por versão e digest do Docker Hub;
atualizações exigem revisão e repetição do preflight.

Somente Caddy publica portas e termina HTTPS com certificado automático. rclone
fica na rede Docker interna `dav`, sem porta pública. Os healthchecks verificam
resposta 401 do DAV e API local do Caddy; não provam DNS, certificado público,
escrita nem espaço livre. Verifique o protocolo pelo endereço público:

```sh
DAV_URL=https://SEU_DOMINIO DAV_USER=SUA_CONTA ./preflight.sh
```

O script pede a senha, cria coleção UUID descartável e verifica PROPFIND 207,
PUT, MOVE 201 e rejeição 412 de sobrescrita com `Overwrite: F`, conferindo que
ambos os conteúdos foram preservados. Execute antes de habilitar sincronização
e após upgrades. Não use `curl -k`. Para curl manual, `curl --user SUA_CONTA -X
PROPFIND -H 'Depth: 1' https://SEU_DOMINIO/` pede a senha sem colocá-la no comando.

## Isolamento e limites

Esta implantação tem **uma conta e uma raiz dedicadas a uma pessoa**. Adicionar
linhas ao htpasswd não separa arquivos: todos teriam acesso à mesma raiz. Para
outra pessoa, use outra instância rclone, raiz, credencial e domínio isolados.
Nenhum serviço próprio de autenticação é necessário. A credencial concede
leitura, escrita e exclusão; a imutabilidade é uma regra dos clientes, não WORM.
Não exponha o diretório de dados por outro serviço nem grave nele com o DAV ativo.

Não há quota por pessoa implementada aqui. `--vfs-disk-space-total-size` não é
quota real. Monitore `df -h`, `df -i`, tamanho de `data/` e logs; reserve espaço
para SO, temporários, snapshots completos e histórico. Para um limite rígido,
use filesystem/dataset dedicado com quota administrada no host. Falha de upload
ou disco cheio deve manter o trabalho local e permitir nova tentativa; não
publique manifesto de snapshot incompleto. Retenção/GC exige análise de todos
os ramos e dispositivos, inclusive offline; não apague versões automaticamente
por idade neste exemplo.

## Backup e restauração

Sincronização não substitui backup. Faça cópia **fora do VPS**, criptografada,
com retenção e teste periódico de restauração. Inclua toda a raiz `data/`
(snapshots, manifestos DAG, histórico, temporários), arquivos desta implantação,
`.env`, htpasswd e volumes `caddy_data`/`caddy_config` (estes incluem chaves).
Proteja esses arquivos como credenciais. Certificados podem ser reemitidos,
mas isso depende de DNS, ACME e limites de emissão.

Para capturar `data/` com consistência, pause os clientes, pare primeiro Caddy e
depois rclone e crie um snapshot do filesystem; só então retome o serviço.
Alternativa simples, com indisponibilidade durante toda a cópia:

```sh
(
set -eu
# BACKUP_DIR deve existir fora de data/, idealmente em outro disco.
: "${BACKUP_DIR:?Defina o destino do backup}"
stamp=$(date -u +%Y%m%dT%H%M%SZ)
trap 'docker compose up -d' EXIT
docker compose stop caddy webdav
sudo tar --acls --xattrs --numeric-owner -czf "$BACKUP_DIR/data-$stamp.tar.gz.part" data
sync -f "$BACKUP_DIR/data-$stamp.tar.gz.part"
mv "$BACKUP_DIR/data-$stamp.tar.gz.part" "$BACKUP_DIR/data-$stamp.tar.gz"
)
```

Não publique `.part` como backup válido. O rename deve ocorrer no mesmo
filesystem; transfira depois para armazenamento externo e confira checksum.
Backup online por simples `tar`/`rsync` pode capturar manifesto sem seu snapshot.
Ao capturar também os volumes Caddy, pare ambos os serviços durante a captura
completa. Um snapshot local sozinho não protege contra perda do VPS.

Para restaurar: pause todos os clientes; pare Caddy e rclone; valide checksum e
liste o arquivo; extraia em pasta vazia de staging (nunca sobre a raiz ativa).
Confira permissões, manifestos e hashes dos snapshots. Preserve a raiz anterior,
renomeie a pasta restaurada para `data/` no mesmo filesystem e restaure as
configurações/credenciais necessárias. Suba os serviços, execute preflight e só
então reconecte um cliente para conferir o histórico. Os outros clientes devem
reconciliar novamente com essa raiz; preserve trabalho local posterior ao backup
como ramo separado. Repetir uploads cegamente pode reintroduzir dados removidos.
