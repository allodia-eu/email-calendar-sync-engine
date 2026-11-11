<?php

/*
 * Seeds the calendar-proxy delegation fixture, once, when the database is created.
 *
 * Two principals make Alice their calendar proxy (the CalendarServer extension SabreDAV
 * implements: a principal's `calendar-proxy-read` / `calendar-proxy-write` group, whose
 * members it reports back on the member as `calendar-proxy-read-for` / `-write-for`):
 *
 *   dave@test.local     Alice is his read proxy: she can read his whole calendar home.
 *   support@test.local  Alice is its write proxy: she can read and write it.
 *
 * Each owns one calendar holding one event — the same fixtures and UIDs the Stalwart harness
 * seeds into the stores it shares (`docker/stalwart/seed/calendar/shared/`), so one
 * assertion can run against both servers.
 *
 * The calendars and events go through SabreDAV's own PDO backend rather than SQL by hand:
 * HarnessBasicAuth admits Alice alone, so they cannot be PUT as their owners, and the
 * backend fills the denormalised columns (etag, occurrence range, sync token) a hand-written
 * INSERT would have to guess.
 */

require_once __DIR__ . '/vendor/autoload.php';

$user = (string) getenv('HARNESS_USER');
$pdo = new PDO('sqlite:' . __DIR__ . '/data/db.sqlite');
$pdo->setAttribute(PDO::ATTR_ERRMODE, PDO::ERRMODE_EXCEPTION);
$calendars = new Sabre\CalDAV\Backend\PDO($pdo);

$principal = $pdo->prepare('INSERT INTO principals (uri, email, displayname) VALUES (?, ?, ?)');
$member = $pdo->prepare(
    'INSERT INTO groupmembers (principal_id, member_id)
     SELECT g.id, m.id FROM principals g, principals m WHERE g.uri = ? AND m.uri = ?'
);

$delegations = [
    // owner, display name, the proxy group Alice joins, calendar, its display name, fixture
    ['dave@test.local', 'Dave Tester', 'calendar-proxy-read', 'shared', 'Dave Shared',
        'dave-shared.ics'],
    ['support@test.local', 'Support Shared Mailbox', 'calendar-proxy-write', 'default',
        'Support', 'support-rota.ics'],
];

foreach ($delegations as [$owner, $name, $proxy, $uri, $displayName, $fixture]) {
    $principal->execute(["principals/{$owner}", $owner, $name]);
    // Both proxy groups exist for every delegating principal, as SabreDAV's own example
    // schema has them for `admin`; Alice joins one.
    $principal->execute(["principals/{$owner}/calendar-proxy-read", null, null]);
    $principal->execute(["principals/{$owner}/calendar-proxy-write", null, null]);
    $member->execute(["principals/{$owner}/{$proxy}", "principals/{$user}"]);

    $id = $calendars->createCalendar(
        "principals/{$owner}",
        $uri,
        ['{DAV:}displayname' => $displayName]
    );
    $data = file_get_contents(__DIR__ . "/seed/calendar/shared/{$fixture}");
    $calendars->createCalendarObject($id, basename($fixture), str_replace("\n", "\r\n", $data));
    echo "seeded {$owner}'s {$uri} calendar; {$user} is its {$proxy}\n";
}
