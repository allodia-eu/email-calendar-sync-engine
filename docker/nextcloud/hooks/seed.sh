#!/bin/sh
# Runs once, as www-data, after the image installs Nextcloud (its post-installation hook) and
# before the web server starts. Seeds the `harness` and `work` calendars with the fixtures the
# Stalwart and SabreDAV harnesses use, so one dataset validates all three servers, gives the
# account an email address, which is what its principal reports as its calendar user address,
# and installs Talk.
set -eu

OCC="php /var/www/html/occ"
USER="$NEXTCLOUD_ADMIN_USER"

$OCC user:setting "$USER" settings email "$HARNESS_EMAIL"

# No login throttling. Suites refuse sign-ins on purpose (a wrong password, the harness password a
# separate server does not take), every request comes from the Docker gateway, and Nextcloud counts
# each refusal against that address: after about ten it delays every login by 25 seconds and its
# Login Flow answers `429`, so the next suite's first sign-in misses its wait. The count outlives a
# run, since the database lives as long as the container.
$OCC config:system:set auth.bruteforce.protection.enabled --type=boolean --value=false
$OCC config:system:set ratelimit.protection.enabled --type=boolean --value=false

$OCC dav:create-calendar "$USER" harness
for ics in /seed/calendar/*.ics; do
    $OCC calendar:import --errors=1 "$USER" harness "$ics"
done
$OCC dav:create-calendar "$USER" work
for ics in /seed/calendar/work/*.ics; do
    $OCC calendar:import --errors=1 "$USER" work "$ics"
done
$OCC dav:create-addressbook "$USER" harness

# Nextcloud Talk, for the meeting-service suite (`live_talk`). The release is pinned by its
# checksum, which is checked before anything is unpacked; bump the two together.
TALK_URL=https://github.com/nextcloud-releases/spreed/releases/download/v25.0.6/spreed-v25.0.6.tar.gz
TALK_SHA256=2b3e3eae42d9bf8ba8da2ab244d16347a3cfacbb8bff346bd99cf32cd6f9c65e
curl -fsSL --retry 3 -o /tmp/spreed.tar.gz "$TALK_URL"
echo "$TALK_SHA256  /tmp/spreed.tar.gz" | sha256sum -c -
tar -xzf /tmp/spreed.tar.gz -C /var/www/html/custom_apps
rm /tmp/spreed.tar.gz
$OCC app:enable spreed

touch /tmp/.nextcloud-harness-ready
