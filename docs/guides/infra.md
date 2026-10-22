# Infra

Databases, brokers and object stores run as containers under `infra`, started before setup, tasks and services and waited on until healthy. A process reaches a container through its host port: `${{ infra.postgres.port }}`.

Two shapes, and most projects mix them: one container shared by every lane with a namespace per lane inside it (a database, a vhost, a Redis index, a bucket), or one container per lane. Shared is cheaper and keeps one copy of the data; per lane isolates versions and lets a lane be wiped by `lane clean`.

devlanes talks to the Docker Engine the `docker` CLI would use (`DOCKER_HOST`, then the current context) and runs `docker compose`. Per-lane Compose services need Compose 2.24.4 or newer for `!override` and `!reset`. `devlanes doctor` shows both versions.

## Compose

```yaml
infra:
  compose: docker-compose.yml           # or [base.yml, dev.yml], merged like repeated -f
  project_name: acme                    # fixed name, so docker compose -p acme works outside devlanes
  exclude: [caddy]                      # never started; the devlanes proxy replaces local reverse proxies
  per_lane: [mailpit]                   # one copy per lane
  profiles: [development]               # Compose profiles to enable
  env: { ACME_DB_PORT: 15432 }          # Compose interpolation only
  hidden: [postgres]                    # not mirrored to machines connected with remote connect
```

The shared project runs from the primary worktree with that worktree's `.env` as `--env-file`, so Compose interpolation (`${ACME_DB_PORT:-15432}`) sees your `.env` plus `infra.env`. Processes do not see `infra.env`; use `global_env` for them.

Published ports of shared services are taken as the file declares them. Two forms work well:

```yaml
services:
  postgres:
    image: postgres:16
    ports: ["127.0.0.1:${ACME_DB_PORT:-15432}:5432"]   # fixed, known outside devlanes too
  redis:
    image: redis:7
    ports: ["127.0.0.1::6379"]                         # ephemeral, Docker picks; devlanes reads it back
```

Both are read back from `docker compose ps`; `infra.redis.port` is the first published port and `infra.postgres.ports['5432']` picks by container port. Give every container a `healthcheck`; `up --wait` returns when they pass, and a service that connects before Postgres accepts connections fails for nothing.

A shared service that `depends_on` an excluded or per-lane service is refused with the pair named. If containers with fixed `container_name`s already exist from a manual `docker compose up`, the same `project_name` adopts them; otherwise the message says which project owns them.

### Per-lane Compose services

`per_lane` services run in their own Compose project, `<project>-lane<N>`, from the lane's worktree. devlanes writes an override file outside the repository that resets `container_name`, replaces `ports` with host ports from the lane's block (every container port the service declares gets one, published or not) and adds `dev.devlanes.*` labels. Named volumes are per project, so each lane has its own data; a volume with a fixed `name:` or `external: true` is shared and gets a warning. Shared dependencies are not started again (`--no-deps`); reach them by published port.

In `setup`, `docker compose` without flags targets the shared project. The lane project's name is `$DEVLANES_COMPOSE_LANE_PROJECT`:

```yaml
infra:
  per_lane: [postgres]
  setup:
    schema: docker compose -p "$DEVLANES_COMPOSE_LANE_PROJECT" exec -T postgres psql -U postgres -d app -c "CREATE TABLE IF NOT EXISTS notes (body text)"
```

## Plain containers

```yaml
infra:
  containers:
    cerbos:
      image: ghcr.io/cerbos/cerbos:0.40.0
      command: [server, --config=/policies/config.yaml]
      volumes: ["./infra/cerbos:/policies:ro"]     # relative to the worktree
      ports: { http: 3592, grpc: 3593 }            # container ports by name; host side allocated
      per_lane: true
      ready: { http: { path: /_cerbos/health, timeout: 45s } }
      env: { CERBOS_LOG_LEVEL: debug }
```

Containers are named `devlanes-<project>-<name>` or `devlanes-<project>-lane<N>-<name>`, bound to `127.0.0.1`, and read as `${{ infra.cerbos.ports.http }}` (`infra.cerbos.port` is the first port). A bare volume name becomes a named volume per project or lane; `/abs` and `./rel` are bind mounts. A running container whose image, command, env, mounts and ports are unchanged is adopted; a changed one is recreated. Readiness is the image's healthcheck, else TCP on the first port, then `ready` if set. Plain containers join Docker's default network, not a Compose network, so address them by host port, not container name.

## Setup and teardown

```yaml
lanes:
  vars:
    db_name: acme_${{ lane.name }}
infra:
  setup:
    database: docker compose exec -T postgres createdb -U postgres ${{ lane.db_name }}
    vhost:
      run: docker compose exec -T rabbitmq rabbitmqctl add_vhost ${{ lane.vhost }}
      container: rabbitmq       # run again when this container restarts (its data may be gone)
      ignore_errors: true       # "already exists" is fine
      timeout: 60s
  teardown:
    database: docker compose exec -T postgres dropdb -U postgres --if-exists --force ${{ lane.db_name }}
```

`setup` entries run once per lane, after its containers are healthy, and are recorded as done; later starts skip them. They run concurrently, so an entry must not depend on another one. A failure (without `ignore_errors`) stops the start and names the entry. `teardown` runs on `devlanes lane clean`, in order, and never on `stop`. Tasks can reuse entries as steps: `steps: [infra.teardown.database, infra.setup.database, migrate, seed]`.

Creating the namespace is `setup` (once); migrating it is a task with `when: lane.start` (every start, idempotent by the migration tool). Seeding is a task with `when: lane.create` and `after: services` when it goes through the API.

## Lifecycle

- `devlanes start` brings infra up for the lane. Shared containers stay up while any lane of the project runs.
- `devlanes stop` stops processes only; containers keep running.
- `devlanes infra down` stops the shared containers (data kept); per-lane containers are untouched. `infra up` and `infra status` are the counterparts.
- `devlanes lane clean <lane> --yes` runs `lane.clean` tasks and `teardown`, then removes the lane's Compose project with its volumes and the lane's plain containers and named volumes. Shared containers and their data survive.
- A profile with `infra: false` skips containers and setup; services that reference `infra.*` then fail to resolve, so keep those references out of the profile's services or point them elsewhere in the profile's `env`.

## Recipes

Each recipe shows the Compose service, the devlanes side, and how a process gets the address. `lane.name` is `main` for lane 0 and `lane<N>` otherwise.

### Postgres, shared, one database per lane

```yaml
# docker-compose.yml
services:
  postgres:
    image: postgres:16
    environment: { POSTGRES_PASSWORD: postgres }
    ports: ["127.0.0.1::5432"]
    volumes: [pgdata:/var/lib/postgresql/data]
    healthcheck: { test: ["CMD-SHELL", "pg_isready -U postgres"], interval: 2s, retries: 30 }
volumes: { pgdata: }
```

```yaml
# devlanes.yaml
lanes:
  vars: { db_name: "acme_${{ lane.name }}" }
infra:
  compose: docker-compose.yml
  setup:
    database: docker compose exec -T postgres createdb -U postgres ${{ lane.db_name }}
  teardown:
    database: docker compose exec -T postgres dropdb -U postgres --if-exists --force ${{ lane.db_name }}
global_env:
  DATABASE_URL: postgres://postgres:postgres@127.0.0.1:${{ infra.postgres.port }}/${{ lane.db_name }}
tasks:
  migrate: { run_script: package.json#db:migrate, when: lane.start }
```

Extensions or roles the app expects go into the same `setup` entry (`psql -c "CREATE EXTENSION ..."`) or into the migration.

### Postgres, one container per lane

For version pinning per branch, or when migrations are destructive:

```yaml
infra:
  compose: docker-compose.yml
  per_lane: [postgres]
  setup:
    schema: docker compose -p "$DEVLANES_COMPOSE_LANE_PROJECT" exec -T postgres psql -U postgres -c "CREATE DATABASE app"
global_env:
  DATABASE_URL: postgres://postgres:postgres@127.0.0.1:${{ infra.postgres.port }}/app
```

Each lane's Postgres gets a port from the lane's block and its own `pgdata` volume. `lane clean` deletes it.

### MySQL and MariaDB

```yaml
# docker-compose.yml
services:
  mysql:
    image: mysql:8
    environment: { MYSQL_ROOT_PASSWORD: root }
    ports: ["127.0.0.1::3306"]
    volumes: [mysqldata:/var/lib/mysql]
    healthcheck: { test: ["CMD", "mysqladmin", "ping", "-h", "127.0.0.1", "-proot"], interval: 2s, retries: 30 }
```

```yaml
infra:
  setup:
    database: docker compose exec -T mysql mysql -uroot -proot -e "CREATE DATABASE IF NOT EXISTS ${{ lane.db_name }}"
  teardown:
    database: docker compose exec -T mysql mysql -uroot -proot -e "DROP DATABASE IF EXISTS ${{ lane.db_name }}"
global_env:
  DATABASE_URL: mysql://root:root@127.0.0.1:${{ infra.mysql.port }}/${{ lane.db_name }}
```

### Redis, one database index per lane

```yaml
lanes:
  vars: { redis_db: "${{ lane.index }}" }
infra:
  compose: docker-compose.yml
  teardown:
    redis: docker compose exec -T redis redis-cli -n ${{ lane.redis_db }} FLUSHDB
global_env:
  REDIS_URL: redis://127.0.0.1:${{ infra.redis.port }}/${{ lane.redis_db }}
```

Redis has 16 databases by default; raise `databases` in its config for more lanes, or put Redis in `per_lane` instead. Libraries that use `SELECT` poorly (some BullMQ setups) prefer a key prefix: `BULL_PREFIX: acme_${{ lane.name }}`.

### RabbitMQ, one vhost per lane

```yaml
lanes:
  vars: { vhost: "${{ 'acme' if lane.index == 0 else 'acme_' ~ lane.name }}" }
infra:
  setup:
    vhost:
      run: |
        docker compose exec -T rabbitmq rabbitmqctl add_vhost ${{ lane.vhost }} || true
        docker compose exec -T rabbitmq rabbitmqctl set_permissions -p ${{ lane.vhost }} acme '.*' '.*' '.*'
      container: rabbitmq
  teardown:
    vhost: { run: "docker compose exec -T rabbitmq rabbitmqctl delete_vhost ${{ lane.vhost }}", ignore_errors: true }
global_env:
  AMQP_URL: amqp://acme:acme@127.0.0.1:${{ infra.rabbitmq.port }}/${{ lane.vhost }}
```

`container: rabbitmq` reruns the entry when the container was recreated, since vhosts live in its volume. Route the management UI through the proxy: `infra.hosts: { rabbitmq: ["rabbit.${{ lane.domain }}"] }` with the 15672 port published first, or with `port: "15672"`.

### MinIO or another S3, one bucket per lane

```yaml
lanes:
  vars: { bucket: "media-${{ lane.name }}" }
infra:
  hosts:
    minio: ["s3.${{ lane.domain }}"]              # presigned URLs the browser can open
  setup:
    bucket:
      run: [aws, --endpoint-url, "http://127.0.0.1:${{ infra.minio.port }}", s3, mb, "s3://${{ lane.bucket }}"]
      ignore_errors: true
  teardown:
    bucket:
      run: [aws, --endpoint-url, "http://127.0.0.1:${{ infra.minio.port }}", s3, rb, "s3://${{ lane.bucket }}", --force]
      ignore_errors: true
global_env:
  S3_ENDPOINT: http://127.0.0.1:${{ infra.minio.port }}
  S3_PUBLIC_ENDPOINT: https://s3.${{ lane.domain }}
  S3_BUCKET: ${{ lane.bucket }}
```

The `aws` or `mc` CLI must be on the PATH of the shell that runs `devlanes start`. `infra.hosts` works for any container with an HTTP port: MinIO's console, Mailpit, pgweb, a mock OIDC provider.

### Mailpit, one per lane

```yaml
# docker-compose.yml
services:
  mailpit:
    image: axllent/mailpit
    ports: ["127.0.0.1::8025", "127.0.0.1::1025"]
```

```yaml
infra:
  per_lane: [mailpit]
  hosts:
    mailpit: ["mail.${{ lane.domain }}"]          # first published port, the web UI
global_env:
  SMTP_HOST: 127.0.0.1
  SMTP_PORT: ${{ infra.mailpit.ports['1025'] }}
```

Each lane's outgoing mail lands in its own inbox at `https://mail.acme-2.localhost`.

### A plain container per lane

```yaml
infra:
  containers:
    oidc:
      image: ghcr.io/navikt/mock-oauth2-server:2.1.10
      ports: { http: 8080 }
      per_lane: true
      env: { JSON_CONFIG_PATH: /config.json }
      volumes: ["./infra/oidc.json:/config.json:ro"]
  hosts:
    oidc: ["auth.${{ lane.domain }}"]
global_env:
  OIDC_ISSUER: https://auth.${{ lane.domain }}/default
```

Anything with an image and a port fits this shape when a Compose file would be overkill.

## Checking

```sh
devlanes infra status          # containers, health and ports
devlanes infra up              # start infra without the services
devlanes env show --explain    # what infra.* resolved to, through global_env
docker compose -p acme ps      # the shared project by name
docker ps --filter label=dev.devlanes.project=acme
```
