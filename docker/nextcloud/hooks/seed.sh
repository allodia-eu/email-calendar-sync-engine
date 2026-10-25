#!/bin/sh
# Runs once, as www-data, after the image installs Nextcloud (its post-installation hook) and
# before the web server starts. Seeds the `harness` calendar with the six calendar fixtures the
# Stalwart and SabreDAV harnesses use, so one dataset validates all three servers, and gives the
# account an email address, which is what its principal reports as its calendar user address.
set -eu

OCC="php /var/www/html/occ"
USER="$NEXTCLOUD_ADMIN_USER"

$OCC user:setting "$USER" settings email "$HARNESS_EMAIL"

$OCC dav:create-calendar "$USER" harness
for ics in /seed/calendar/*.ics; do
    $OCC calendar:import --errors=1 "$USER" harness "$ics"
done
$OCC dav:create-addressbook "$USER" harness

touch /tmp/.nextcloud-harness-ready
