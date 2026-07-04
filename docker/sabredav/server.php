<?php

/*
 * Minimal, deterministic SabreDAV CalDAV server for the PIM-engine test harness.
 *
 * It is the second protocol fixture beside the Stalwart one (`docker/stalwart/`),
 * giving `provider-caldav` a different real CalDAV implementation to validate
 * against — SabreDAV is what Soverin/Fastmail-style stacks run, and it diverges
 * from Stalwart in exactly the ways that matter (two-step RFC 6764 discovery, the
 * `http://sabre.io/ns/sync/N` sync-token form, calendar collection naming).
 *
 * Based on SabreDAV's own examples/calendarserver.php, with two harness changes:
 *   1. HTTP Basic auth against one throwaway account (the engine client uses
 *      Basic, not the stock PDO backend's Digest).
 *   2. A `/.well-known/caldav` redirect, since the PHP built-in server has no
 *      rewrite rules of its own.
 *
 * Credentials come from the environment (HARNESS_USER / HARNESS_PASS) and are
 * throwaway test values — this server never holds real data.
 */

date_default_timezone_set('UTC');
require_once __DIR__ . '/vendor/autoload.php';

// RFC 6764 §5: the well-known URI redirects to the DAV context root. A real
// deployment does this in the web server; the PHP built-in server cannot, so we
// handle it here so the engine's default discovery path works out of the box.
$path = parse_url($_SERVER['REQUEST_URI'] ?? '/', PHP_URL_PATH);
if ('/.well-known/caldav' === $path || '/.well-known/carddav' === $path) {
    header('Location: /', true, 301);
    exit;
}

$pdo = new PDO('sqlite:' . __DIR__ . '/data/db.sqlite');
$pdo->setAttribute(PDO::ATTR_ERRMODE, PDO::ERRMODE_EXCEPTION);

/** HTTP Basic auth against the single seeded harness account. */
class HarnessBasicAuth extends Sabre\DAV\Auth\Backend\AbstractBasic
{
    protected function validateUserPass($username, $password)
    {
        return hash_equals((string) getenv('HARNESS_USER'), (string) $username)
            && hash_equals((string) getenv('HARNESS_PASS'), (string) $password);
    }
}

$authBackend = new HarnessBasicAuth();
$authBackend->setRealm('SabreDAV harness');
$calendarBackend = new Sabre\CalDAV\Backend\PDO($pdo);
$principalBackend = new Sabre\DAVACL\PrincipalBackend\PDO($pdo);

$tree = [
    new Sabre\CalDAV\Principal\Collection($principalBackend),
    new Sabre\CalDAV\CalendarRoot($principalBackend, $calendarBackend),
];

$server = new Sabre\DAV\Server($tree);
$server->setBaseUri('/');

$server->addPlugin(new Sabre\DAV\Auth\Plugin($authBackend));
$server->addPlugin(new Sabre\DAVACL\Plugin());
$server->addPlugin(new Sabre\CalDAV\Plugin());
// WebDAV-Sync (RFC 6578): the sync-token the engine syncs incrementally against.
$server->addPlugin(new Sabre\DAV\Sync\Plugin());

$server->start();
