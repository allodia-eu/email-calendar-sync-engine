# SabreDAV CalDAV test fixture. PHP + Composer + sabre/dav over a SQLite backend,
# served by the PHP built-in web server. The build context is `docker/` (set in
# docker-compose.yml) so this image can reuse the shared calendar seed under
# `docker/stalwart/seed/calendar` — one dataset validates both protocol fixtures.
#
# The base image is pinned by multi-arch digest and the PHP deps by the committed
# composer.lock, so a rebuild is deterministic (mirroring the Stalwart compose's
# digest pin). Bump deliberately: re-resolve the digest / regenerate the lock.
FROM php:8.3-cli-bookworm@sha256:4f520b09238c0ca8ee5d91367c7e4ba9a7db309ec969ef901d4599facaccf537

# sabre/dav needs mbstring + a PDO driver; the sqlite3 CLI seeds the schema.
RUN apt-get update && apt-get install -y --no-install-recommends \
        libonig-dev libsqlite3-dev sqlite3 curl unzip git ca-certificates \
    && docker-php-ext-install mbstring pdo_sqlite \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY --from=composer:2 /usr/bin/composer /usr/bin/composer

WORKDIR /app
# Deterministic install from the committed lockfile (sabre/dav 4.6.0 + deps).
COPY sabredav/composer.json sabredav/composer.lock ./
RUN composer install --no-interaction --no-progress

COPY sabredav/server.php sabredav/entrypoint.sh sabredav/seed.sh ./
# SabreDAV's composer dist excludes examples/, so the schema is vendored here.
COPY sabredav/sql ./sql
COPY stalwart/seed/calendar ./seed/calendar
RUN chmod +x entrypoint.sh seed.sh

EXPOSE 8080
ENTRYPOINT ["sh", "./entrypoint.sh"]
