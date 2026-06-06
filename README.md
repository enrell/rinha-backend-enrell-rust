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
3. Cada vetor vira `QVec = [i16; 16]` com `SCALE = 10000`.
4. Os vetores são ordenados por `partition_key` de 8 bits.
5. Cada partição recebe uma KD/BBox tree exata.
6. O índice serializado é copiado para `/app/data`.

Arquivos gerados:

| Arquivo | Conteúdo |
| --- | --- |
| `vectors.bin` | Vetores quantizados contíguos |
| `labels.bin` | Labels `0=legit`, `1=fraud` |
| `partitions.bin` | Metadados das 256 partições |
| `nodes.bin` | Nós KD/BBox serializados |

A busca em runtime é exata: usa lower bound de bounding box para podar partições/nós, mas não descarta candidatos sem prova por distância.

## Arquitetura do Código

| Arquivo | Responsabilidade |
| --- | --- |
| `src/index.rs` | Formato compartilhado do índice, quantização e partition key |
| `src/search.rs` | Carregamento via mmap e busca exata KD/BBox AVX2 |
| `src/fdpass_common.rs` | Tipos comuns de `sendmsg`/`recvmsg` |
| `src/fdpass_send.rs` | Envio de fd usado pelo `lb` |
| `src/fdpass_recv.rs` | Recepção de fd usada pela `api` |

O projeto não usa dependências externas. HTTP, JSON, fd-pass, epoll, mmap e SIMD são implementados diretamente com std/raw syscalls.

## Endpoints

| Método | Rota | Resposta |
| --- | --- | --- |
| `GET` | `/ready` | `200` sem body |
| `POST` | `/fraud-score` | `{"approved": bool, "fraud_score": number}` |

Rotas desconhecidas retornam `404`.

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
2. `indexer`: descompacta `references.json.gz` e gera `/app/data`.
3. runtime `alpine`: contém apenas binários e índice final.

## Limites de Recursos

O `docker-compose.yml` soma exatamente `1 CPU` e `350 MB`:

| Serviço | CPU | Memória |
| --- | ---: | ---: |
| `lb` | `0.20` | `20MB` |
| `api1` | `0.40` | `165MB` |
| `api2` | `0.40` | `165MB` |

A rede usa `bridge`, e as imagens são configuradas para `linux/amd64`.

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

Resultados locais observados antes desta limpeza:

| Teste | Resultado |
| --- | --- |
| k6 oficial completo | `0` FP, `0` FN, `0` HTTP errors |
| p99 oficial local | `~0.83ms` |
| score local | `6000` |

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
