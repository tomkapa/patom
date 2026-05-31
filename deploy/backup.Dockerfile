# Backup sidecar image: pg_dump (v17, matches pgvector/pgvector:pg17) + aws CLI
# for shipping dumps to Cloudflare R2 (S3-compatible). Used by the Postgres
# backup CronJob in deploy/helm/patom. Push as ghcr.io/tomkapa/patom-backup.
FROM postgres:17-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends awscli ca-certificates \
 && rm -rf /var/lib/apt/lists/*
# The CronJob supplies the command (pg_dump | aws s3 cp ...); no ENTRYPOINT here
# beyond the base image's. Runs as the postgres user already present in the image.
USER postgres
