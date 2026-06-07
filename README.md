# Rinha de Backend 2026 - Rust Fraud Detection

Backend em Rust para a Rinha de Backend 2026, com foco em baixa latência e busca vetorial exata para detecção de fraude.

## Visão Geral

A solução usa dois binários principais e um preprocessador de índice:

| Binário | Arquivo | Função |
| --- | --- | --- |
| `lb` | `src/bin/lb.rs` | Load balancer TCP na porta `9999`, round-robin puro |
| `api` | `src/bin/api.rs` | Worker HTTP que processa `/ready` e `/fraud-score` |
| `preprocess` | `src/bin/preprocess.rs` | Gera o índice vetorial a partir de `references.json.gz` |

Fluxo em runtime:

```text
cliente -> lb :9999 -> fd-pass Unix socket -> api -> busca vetorial -> resposta JSON
```

O load balancer não faz parsing HTTP nem inspeciona payload. Ele aceita conexões TCP e repassa os file descriptors para as APIs via Unix socket.

## Pipeline Vetorial

O índice é pré-processado no build da imagem Docker:

1. `references.json.gz` é descompactado no stage `indexer`.
2. `preprocess` parseia os 3M vetores de referência.
3. Cada vetor vira `QVec = [i16; 16]` com `SCALE = 10000`: 14 dimensões reais mais 2 posições de padding para SIMD.
4. Os vetores são ordenados por `partition_key` de 8 bits.
5. Cada partição recebe uma KD/BBox tree exata.
6. O `index.bin` runtime é gravado em layout staged: `hot4`, `mid4`, `cold8`, labels e metadados.
7. O índice serializado é copiado para `/index/index.bin`.

Arquivo gerado no modo padrão:

| Arquivo | Conteúdo |
| --- | --- |
| `index.bin` | Índice único usado em runtime via `INDEX_PATH`, com layout `hot4/mid4/cold8` |

Com `WRITE_LEGACY_INDEX=1`, o preprocessador também emite arquivos separados para inspeção/debug:

| Arquivo | Conteúdo |
| --- | --- |
| `vectors.bin` | Vetores quantizados contíguos |
| `labels.bin` | Labels `0=legit`, `1=fraud` |
| `partitions.bin` | Metadados das 256 partições |
| `nodes.bin` | Nós KD/BBox serializados |

A busca em runtime é exata: usa lower bound de bounding box para podar partições/nós e lower bound por soma parcial (`hot4`, depois `hot4+mid4`) para descartar candidatos individuais sem tocar o restante do vetor quando isso já não pode entrar no top-5.

## Arquitetura do Código

| Arquivo | Responsabilidade |
| --- | --- |
| `src/index.rs` | Formato compartilhado do índice, quantização e partition key |
| `src/search.rs` | Carregamento via mmap e busca exata KD/BBox com staged lower-bound |
| `src/fdpass_common.rs` | Tipos comuns de `sendmsg`/`recvmsg` |
| `src/fdpass_send.rs` | Envio de fd usado pelo `lb` |
| `src/fdpass_recv.rs` | Recepção de fd usada pela `api` |

O projeto não usa dependências externas. HTTP, JSON, fd-pass, epoll, mmap e SIMD são implementados diretamente com std/raw syscalls.

## Endpoints

| Método | Rota | Resposta |
| --- | --- | --- |
| `GET` | `/ready` | `200` sem body |
| `POST` | `/fraud-score` | `{"approved": bool, "fraud_score": number}` |

Rotas desconhecidas retornam `404`. Payload inválido, body ausente ou JSON não parseável em `/fraud-score` cai no fallback rápido `200 {"approved":true,"fraud_score":0.0}`.

## Build Local

```bash
cargo build --release --bin lb --bin api --bin preprocess
```

Para gerar um índice pequeno de teste:

```bash
cargo run --bin preprocess -- resources/example-references.json /tmp/rinha-index
```

## Docker

```bash
docker compose up -d --build
```

O `Dockerfile` faz build em múltiplos estágios:

1. `builder`: compila `lb`, `api` e `preprocess`.
2. `indexer`: descompacta `references.json.gz` e gera `/app/index`.
3. runtime `alpine`: contém apenas binários e `/index/index.bin`.

## Limites de Recursos

O `docker-compose.yml` soma exatamente `1 CPU` e `350 MB`:

| Serviço | CPU | Memória |
| --- | ---: | ---: |
| `lb` | `0.20` | `20MB` |
| `api1` | `0.40` | `165MB` |
| `api2` | `0.40` | `165MB` |

A rede usa `bridge`, e as imagens são configuradas para `linux/amd64`. O build usa `target-cpu=haswell` e a busca faz `assert` de AVX2 em runtime; a imagem pressupõe CPU x86_64 com AVX2.

O compose deixa afinidade ampla por padrão para evitar regressões de scheduler local, mas permite fixar via env:

| Serviço | `cpuset` |
| --- | --- |
| `lb` | `${LB_CPUSET:-0,1,2,3}` |
| `api1` | `${API1_CPUSET:-0,1,2,3}` |
| `api2` | `${API2_CPUSET:-0,1,2,3}` |

Variáveis principais de runtime:

| Variável | Serviço | Padrão/uso |
| --- | --- | --- |
| `LISTEN_ADDR` | `lb` | Endereço TCP, padrão `0.0.0.0:9999` |
| `BACKEND_SOCKS` | `lb` | Unix sockets das APIs, separados por vírgula |
| `FD_PASS_PATH` | `api` | Unix socket onde a API recebe file descriptors |
| `INDEX_PATH` | `api` | Caminho do índice único, no compose `/index/index.bin` |
| `INDEX_HUGE` | `api` | Copia o mmap para região anônima com hint de huge pages quando verdadeiro |
| `INDEX_MLOCK` | `api` | Tenta travar o índice em memória com `mlock` quando verdadeiro |
| `INDEX_PRIMARY_ONLY` | `api` | Se verdadeiro, busca só a partição primária |
| `INDEX_MAX_EXTRA_PARTITIONS` | `api` | Limite de partições extras candidatas por bbox, padrão do compose `16` |
| `INDEX_STATS` | `api` | Liga logs de estatísticas por request |
| `MAX_CLIENTS` | `api` | Tamanho do pool de conexões por worker, padrão `1024` |
| `DATA_DIR` | `api` | Fallback para índices separados quando `INDEX_PATH` não existe, padrão `/app/data` |

As APIs usam `INDEX_PATH=/index/index.bin`, `INDEX_HUGE=1`, `INDEX_MLOCK=1` e `ulimits.memlock=-1` no compose padrão.

## Benchmarks

Smoke local:

```bash
./bench/run.sh smoke
```

Benchmark rápido a 900 rps:

```bash
./bench/run.sh quick
```

Benchmark com payloads variados, usando o checkout oficial em `../rinha-de-backend-2026/test`:

```bash
./bench/run.sh mini
```

Resultados de benchmark variam bastante por host, kernel, Docker e cliente de carga. Para comparar submissões, use o `test/test.js` do repositório oficial com a imagem pública da branch `submission`; os scripts em `bench/` são auxiliares locais.

## Instrumentação

Os contadores internos da busca ficam desligados por padrão. Para logar uma linha por request em `/fraud-score`:

```bash
INDEX_STATS=true docker compose up -d --build
```

Cada linha `search_stats` inclui:

| Campo | Significado |
| --- | --- |
| `key` | `partition_key` da query |
| `fraud_count` | Quantos dos top-5 vizinhos são fraude |
| `search_ns` | Tempo da busca vetorial em nanossegundos |
| `partitions_considered` | Partições não vazias avaliadas por bbox, incluindo a exata |
| `partitions_searched` | Partições efetivamente abertas |
| `partitions_pruned` | Partições descartadas por lower bound |
| `empty_partitions` | Partições vazias ignoradas |
| `nodes_visited` | Nós KD/BBox visitados |
| `nodes_pruned` | Nós podados por lower bound |
| `leaves_scanned` | Folhas escaneadas |
| `vectors_scanned` | Vetores comparados com distância exata |
| `stage8_pruned` | Candidatos podados depois do lower bound `hot4+mid4` |
| `full_scanned` | Candidatos que chegaram ao cálculo completo das 16 posições armazenadas |

## Submissão

A branch `submission` deve conter apenas os arquivos necessários para execução do teste, sem código-fonte. O `docker-compose.yml` de submissão deve apontar para uma imagem pública, não para `build: .`.

Arquivos esperados para submissão:

| Arquivo | Observação |
| --- | --- |
| `docker-compose.yml` | Na raiz da branch `submission` |
| `info.json` | Metadados da participação |
| configs necessários | Somente se usados |

## Licença

MIT. Veja [`LICENSE`](./LICENSE).
