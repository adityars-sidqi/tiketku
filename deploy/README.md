# Infrastruktur lokal TiketKu

Satu perintah menjalankan semua infrastruktur pendukung: PostgreSQL, Redis, Kafka (KRaft), RabbitMQ, Mailpit, dan Jaeger. Service Rust dijalankan terpisah (`cargo run`).

## Prasyarat

- Podman 4.9+ dengan provider Docker Compose v2 (`sudo apt install -y podman docker-compose-v2`, lalu `systemctl --user enable --now podman.socket`). Docker tidak dibutuhkan; lihat `docs/adr/0001-podman-k3s-free-tooling.md`.
- Jalankan semua perintah dari root repo.

## Menjalankan

```bash
cp .env.example .env        # sekali saja; .env diabaikan git
podman compose --env-file .env -f deploy/docker-compose.yml up -d --wait
```

`--env-file .env` wajib karena Compose hanya memuat `.env` otomatis dari folder yang sama dengan file compose (`deploy/`), sedangkan `.env` ada di root repo. `--wait` menunggu sampai semua container healthy.

Dengan Kafka UI (opsional, profile `tools`):

```bash
podman compose --env-file .env -f deploy/docker-compose.yml --profile tools up -d --wait
```

Cek status: `podman compose --env-file .env -f deploy/docker-compose.yml ps`

## Menghentikan

```bash
podman compose --env-file .env -f deploy/docker-compose.yml down
```

Data tetap tersimpan di named volume dan dipakai lagi pada `up` berikutnya.

## Alamat dan port

| Komponen | Alamat host | Keterangan |
| --- | --- | --- |
| PostgreSQL | `localhost:5432` | 5 database: `auth_db`, `catalog_db`, `order_db`, `payment_db`, `notification_db` |
| Redis | `localhost:6379` | Butuh password (`REDIS_PASSWORD`) |
| Kafka | `localhost:9092` | Listener host; antar container pakai `kafka:19092` |
| Kafka UI | http://localhost:8090 | Hanya dengan `--profile tools` |
| RabbitMQ AMQP | `localhost:5672` | vhost `tiketku` |
| RabbitMQ UI | http://localhost:15672 | Login dengan `RABBITMQ_USER` / `RABBITMQ_PASSWORD` |
| Mailpit SMTP | `localhost:1025` | Terima email tanpa autentikasi |
| Mailpit UI | http://localhost:8025 | Lihat email yang terkirim |
| Jaeger UI | http://localhost:16686 | Trace |
| OTLP gRPC / HTTP | `localhost:4317` / `localhost:4318` | Tujuan exporter OpenTelemetry |

Semua port dan versi image bisa diubah di `.env` (nama variabelnya ada di `.env.example`).

Contoh connection string (nilai default):

```
postgres://tiketku:tiketku_dev_password@localhost:5432/auth_db
redis://:tiketku_dev_password@localhost:6379
amqp://tiketku:tiketku_dev_password@localhost:5672/tiketku
```

## Catatan

- Topic Kafka **tidak** dibuat otomatis (auto-create dimatikan); topic dibuat lewat story #31.
- Topologi RabbitMQ (exchange dan queue) dideklarasikan oleh service sendiri; infrastruktur hanya menyediakan broker, user, dan vhost.
- Script init Postgres (`deploy/compose/postgres/init-databases.sh`) hanya berjalan saat volume data masih kosong. Script aman dijalankan ulang karena tiap database hanya dibuat jika belum ada.
- Data Jaeger dan Mailpit disimpan di memori, jadi hilang saat container di-restart.
- Mengubah `.env` (mis. password) setelah volume terisi tidak mengubah user di database yang sudah ada; reset data dulu.

## Reset data

**Peringatan: ini menghapus semua data lokal (database, Redis, Kafka, RabbitMQ).**

```bash
podman compose --env-file .env -f deploy/docker-compose.yml --profile tools down -v
```

Setelah itu jalankan `up -d --wait` lagi; script init Postgres akan membuat ulang kelima database. Untuk mereset satu komponen saja, hentikan dulu servicenya lalu hapus volume-nya (`podman volume ls` untuk melihat nama, awalan `tiketku_`).
