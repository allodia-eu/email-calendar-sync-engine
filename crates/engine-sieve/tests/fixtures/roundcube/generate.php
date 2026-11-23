<?php
// Builds rule sets the way Roundcube's managesieve plugin's forms do (its rule array), and prints
// what the plugin writes for each, base64-encoded so CR LF survive, beside what it writes again
// after reading its own output back. It calls the plugin and copies none of it; `README.md` says
// how to run it.
require '/usr/src/roundcubemail/plugins/managesieve/lib/Roundcube/rcube_sieve_script.php';
$caps = ['fileinto','reject','envelope','vacation','imap4flags','copy','body','relational','regex','mailbox','date','variables','editheader','include','subaddress','index','mime','special-use','mailboxid','duplicate','enotify','vacation-seconds'];
function rule($name, $tests, $actions, $join = false, $disabled = false) {
    return ['type' => 'if', 'tests' => $tests, 'actions' => $actions, 'join' => $join, 'disabled' => $disabled, 'name' => $name];
}
$sets = [];
$sets['one_rule'] = [
    rule('Invoices', [['test' => 'header', 'not' => false, 'arg1' => 'subject', 'arg2' => 'Invoice', 'type' => 'contains']],
         [['type' => 'fileinto', 'target' => 'Finance'], ['type' => 'stop']]),
];
$sets['disabled_rule'] = [
    rule('Board', [['test' => 'address', 'not' => false, 'arg1' => 'from', 'arg2' => 'board@example.com', 'type' => 'is', 'part' => 'all']],
         [['type' => 'fileinto', 'copy' => true, 'target' => 'Board']], false, true),
    rule('Newsletters', [['test' => 'header', 'not' => false, 'arg1' => 'list-id', 'arg2' => 'news', 'type' => 'contains'],
                         ['test' => 'address', 'not' => true, 'arg1' => 'from', 'arg2' => 'example.net', 'type' => 'is', 'part' => 'domain']],
         [['type' => 'fileinto', 'target' => 'Trash']], true, true),
];
$sets['many_rules'] = [
    rule('Any of two', [['test' => 'header', 'not' => false, 'arg1' => 'subject', 'arg2' => ['urgent', 'asap'], 'type' => 'contains'],
                        ['test' => 'address', 'not' => false, 'arg1' => ['to', 'cc'], 'arg2' => 'example.org', 'type' => 'is', 'part' => 'domain']],
         [['type' => 'addflag', 'target' => '\\Flagged'], ['type' => 'addflag', 'target' => ['\\Seen', '$label1']]], false),
    rule('All of two', [['test' => 'header', 'not' => false, 'arg1' => 'subject', 'arg2' => 'report*', 'type' => 'matches'],
                        ['test' => 'header', 'not' => true, 'arg1' => 'x-spam-flag', 'arg2' => 'YES', 'type' => 'is']],
         [['type' => 'redirect', 'copy' => true, 'target' => 'archive@example.com']], true),
    rule('Big', [['test' => 'size', 'not' => false, 'arg' => '5M', 'type' => 'over']],
         [['type' => 'discard']]),
    rule('', [['test' => 'body', 'not' => false, 'part' => 'text', 'arg' => 'unsubscribe', 'type' => 'contains']],
         [['type' => 'fileinto', 'target' => 'Lists'], ['type' => 'stop']]),
    rule('Regex', [['test' => 'header', 'not' => false, 'arg1' => 'subject', 'arg2' => '^\[ticket-[0-9]+\]', 'type' => 'regex']],
         [['type' => 'setflag', 'target' => '$ticket']]),
    rule('Everything', [['test' => 'true']], [['type' => 'keep']]),
];
$sets['quoting'] = [
    rule('Quote "and" back\\slash', [['test' => 'header', 'not' => false, 'arg1' => 'subject', 'arg2' => 'say "hi" \\ there', 'type' => 'contains']],
         [['type' => 'fileinto', 'target' => 'Odd "folder"']]),
    rule('Multi line', [['test' => 'body', 'not' => false, 'part' => 'text', 'arg' => "first line\n.second line", 'type' => 'contains']],
         [['type' => 'reject', 'target' => "Not here.\nGo away."]]),
    rule('Unicode é', [['test' => 'header', 'not' => false, 'arg1' => 'subject', 'arg2' => 'Größe', 'type' => 'contains']],
         [['type' => 'fileinto', 'target' => 'Ärger']]),
];
$sets['vacation_rule'] = [
    rule('Out of office', [['test' => 'currentdate', 'not' => false, 'part' => 'date', 'arg' => '2030-01-02', 'type' => 'value-ge'],
                           ['test' => 'currentdate', 'not' => false, 'part' => 'date', 'arg' => '2030-01-09', 'type' => 'value-le']],
         [['type' => 'vacation', 'days' => 1, 'subject' => 'Away', 'from' => 'carol@example.com', 'reason' => "I am away.\nBack on the ninth."]], true),
    rule('Invoices', [['test' => 'header', 'not' => false, 'arg1' => 'subject', 'arg2' => 'Invoice', 'type' => 'contains']],
         [['type' => 'fileinto', 'target' => 'Finance']]),
];
foreach ($sets as $name => $rules) {
    $s = new rcube_sieve_script('', $caps);
    foreach ($rules as $r) { $s->add_rule($r); }
    $text = $s->as_text();
    // And read back by the plugin: the rules it recognises, as it would show them.
    $back = new rcube_sieve_script($text, $caps);
    echo $name, ' ', base64_encode($text), ' ', base64_encode($back->as_text()), "\n";
}
