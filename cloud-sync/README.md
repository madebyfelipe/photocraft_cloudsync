# ArtCraft Cloud Sync — execução local

O cliente compartilhado e os adaptadores desktop usam os formatos nativos existentes. O servidor é um WebDAV pessoal com HTTPS; as instruções de implantação estão em [deploy/README.md](deploy/README.md).

## Obter os dois aplicativos

Mantenha os checkouts irmãos com estes nomes, pois VectorCraft referencia este pacote por caminho relativo:

```sh
git clone https://github.com/madebyfelipe/photocraft_cloudsync.git
git clone https://github.com/madebyfelipe/vectorcraft_cloudsync.git
rustup toolchain install 1.95.0 --profile minimal
```

Para dependências nativas do seu sistema, consulte `docs/development.md` em cada repositório. Em Linux, o cliente de credenciais também requer os arquivos de desenvolvimento DBus (`libdbus-1-dev` em Debian/Ubuntu); um Secret Service disponível permite guardar a senha no cofre do sistema.

No checkout PhotoCraft:

```sh
cd photocraft_cloudsync
cargo +1.95.0 run --locked -p photocraft
```

Em outro terminal, no checkout VectorCraft:

```sh
cd vectorcraft_cloudsync
cargo +1.95.0 run --locked -p vectorcraft
```

Os checkpoints locais funcionam antes de configurar uma conta. PhotoCraft oferece o botão **Cloud Projects**; VectorCraft oferece **File → Cloud Projects…**. Informe o endereço HTTPS, usuário e senha da mesma conta WebDAV dedicada em cada computador. O endpoint e usuário são persistidos; a senha usa o cofre do sistema quando disponível e, caso contrário, vale somente para a sessão.

## Servidor e teste entre computadores

Prepare a stack seguindo [deploy/README.md](deploy/README.md), configure domínio/DNS e execute o preflight pelo endereço HTTPS público. Na VPS informada, mantenha o Cloud Sync em uma pasta própria e confira as portas usadas pelas stacks existentes em `~/automacao`, `~/chatbotx` e `~/uptime-kuma` antes de iniciar outro proxy.

1. Conecte o aplicativo no computador A, crie um documento e faça uma edição; aguarde **Saved**.
2. No computador B, conecte a mesma conta e abra o projeto pela janela Cloud Projects. Confira elementos editáveis e imagens.
3. Edite em A e reabra a revisão mais recente em B; documentos já abertos não são substituídos automaticamente durante a edição.
4. Desconecte A da rede, edite e confira o estado offline. Reconecte e aguarde o envio automático.
5. Edite a mesma base nos dois computadores enquanto estão offline. Após reconectar, confira as duas cópias de conflito e a preservação de ambas.

Para repetir os testes do cliente, execute `cargo +1.95.0 test --locked` nesta pasta. Os testes nativos dos adaptadores e as evidências estão em [../docs/cloud-sync/TESTS.md](../docs/cloud-sync/TESTS.md).

Limites atuais: snapshots de até 256 MiB, histórico sem coleta automática, fontes instaladas em cada computador e originais vinculados completos/incorporados para upload. Mantenha backup externo. A implantação na Contabo e os testes em dois computadores físicos ainda precisam ser realizados.
