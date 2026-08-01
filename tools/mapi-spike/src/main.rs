//! MAPI-over-HTTP spike CLI. Each subcommand is one checkpoint, so each
//! prints its own measurement. See README.md for the scope fence.

#![allow(dead_code)] // scaffolding for later checkpoints

mod connect;
mod cursor;
mod http;
mod proptag;
mod rop;
mod ropbuf;
mod table;
mod transcript;

use std::process::ExitCode;

use connect::{ConnectRequest, ConnectResponse};

const USAGE: &str = "\
mapi-spike — measure what the MAPI ROP/OXCDATA layer costs

USAGE:
  mapi-spike ping    --url <mapi-url> --user <u> --pass <p>
  mapi-spike connect --url <mapi-url> --user <u> --pass <p> --dn <legacy-dn>
  mapi-spike logon   --url <mapi-url> --user <u> --pass <p> --dn <legacy-dn>
  mapi-spike rows    --url <mapi-url> --user <u> --pass <p> --dn <legacy-dn> [--table hierarchy|contents]

  <mapi-url> must include the MailboxId query parameter that Autodiscover
  returns, e.g.
    http://127.0.0.1:18082/mapi/emsmdb/?MailboxId=<guid>@<domain>
  <legacy-dn> is Autodiscover's <User><LegacyDN>.
  --insecure skips TLS verification (Exchange lab installs use self-signed certs).
  --transcript <dir> writes every request/response byte pair to <dir>.
  --scrub <from=to> rewrites a substring in captured bytes (repeatable).
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        eprint!("{USAGE}");
        return ExitCode::FAILURE;
    };

    let url = flag(&args, "--url").unwrap_or_default();
    let user = flag(&args, "--user").unwrap_or_default();
    let pass = flag(&args, "--pass").unwrap_or_default();

    if url.is_empty() || user.is_empty() {
        eprint!("{USAGE}");
        return ExitCode::FAILURE;
    }

    // Exchange Server's MAPI vdir is HTTPS with a lab self-signed cert.
    let insecure = args.iter().any(|a| a == "--insecure");
    let mut session = http::Session::with_tls(url, user, pass, insecure);

    if let Some(dir) = flag(&args, "--transcript") {
        match session.record_to(&dir, scrub_rules(&args)) {
            Ok(path) => println!("capturing transcripts to {}", path.display()),
            Err(e) => {
                eprintln!("cannot write transcripts to {dir}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let result = match cmd.as_str() {
        "ping" => ping(&mut session),
        "connect" | "logon" | "rows" => match flag(&args, "--dn") {
            Some(dn) => {
                let r = do_connect(&mut session, &dn);
                match (cmd.as_str(), r) {
                    ("logon", Ok(_)) => do_logon(&mut session, &dn).map(|_| ()),
                    ("rows", Ok(_)) => {
                        let which = flag(&args, "--table").unwrap_or_else(|| "hierarchy".into());
                        do_logon(&mut session, &dn)
                            .and_then(|logon| do_rows(&mut session, &logon, &which))
                    }
                    (_, r) => r.map(|_| ()),
                }
            }
            None => {
                eprintln!("{cmd} needs --dn (Autodiscover's <User><LegacyDN>)");
                return ExitCode::FAILURE;
            }
        },
        other => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("FAILED: {e}");
            ExitCode::FAILURE
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

/// Every `--scrub from=to`, in order. Split on the *first* `=` so a replacement
/// containing one still works.
fn scrub_rules(args: &[String]) -> Vec<(String, String)> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--scrub")
        .filter_map(|(i, _)| args.get(i + 1))
        .filter_map(|spec| spec.split_once('='))
        .map(|(from, to)| (from.to_owned(), to.to_owned()))
        .collect()
}

/// PING validates an *existing* Session Context, so on a fresh session the
/// expected answer is a missing-cookie error, not success. That still proves
/// the endpoint exists, auth works, and the server speaks MAPI/HTTP.
fn ping(session: &mut http::Session) -> Result<(), Box<dyn std::error::Error>> {
    match session.post("PING", Vec::new()) {
        Ok(resp) => {
            println!("PING ok, X-ResponseCode {}", resp.response_code);
            Ok(())
        }
        Err(http::Error::ResponseCode { code, diagnostic }) if !session.has_session() => {
            println!(
                "PING answered: X-ResponseCode {code} ({})",
                diagnostic.unwrap_or_else(|| "no diagnostic".into())
            );
            println!("  (expected without a session — PING validates an existing Session Context)");
            Ok(())
        }
        Err(e) => Err(Box::new(e)),
    }
}

fn do_connect(
    session: &mut http::Session,
    user_dn: &str,
) -> Result<ConnectResponse, Box<dyn std::error::Error>> {
    let body = ConnectRequest::new(user_dn).serialize();
    println!(
        "Connect request body: {} bytes (auxiliary buffer empty)",
        body.len()
    );

    let resp = session.post("Connect", body)?;
    let parsed = ConnectResponse::parse(&resp.body)?;

    println!("  X-ResponseCode : {}", resp.response_code);
    println!("  meta-tags      : {:?}", resp.meta_tags);
    println!("  StatusCode     : 0x{:08X}", parsed.status_code);
    println!(
        "  ErrorCode      : 0x{:08X} ({})",
        parsed.error_code,
        parsed.error_name()
    );
    println!("  PollsMax       : {}", parsed.polls_max);
    println!("  RetryCount     : {}", parsed.retry_count);
    println!("  RetryDelay     : {}", parsed.retry_delay);
    println!("  DnPrefix       : {}", parsed.dn_prefix);
    println!("  DisplayName    : {}", parsed.display_name);
    println!(
        "  session cookie : {}",
        if session.has_session() { "yes" } else { "NO" }
    );

    if !parsed.ok() {
        return Err(format!("Connect failed: {}", parsed.error_name()).into());
    }
    println!("\nCP2 measurement: AuxiliaryBufferSize=0 ACCEPTED — MS-OXCRPC aux layer deferred.");
    Ok(parsed)
}

/// CP3: the first point where the ROP encoder, the handle table and the OXCDATA
/// primitives all have to be right at the same time.
fn do_logon(
    session: &mut http::Session,
    essdn: &str,
) -> Result<(rop::LogonResponse, u32), Box<dyn std::error::Error>> {
    use rop::{FOLDER_NAMES, RopResponse};

    let mut buf = ropbuf::RopBuffer::new();
    buf.push_rop(&rop::logon_request(0, essdn));
    buf.set_handle(0, ropbuf::HANDLE_NONE);
    let rop_buffer = buf.serialize();

    println!("\nExecute #1: RopLogon");
    println!(
        "  RopBuffer      : {} bytes ({} ROP, {} handle slot)",
        rop_buffer.len(),
        1,
        1
    );

    let resp = session.post("Execute", ropbuf::execute_request(&rop_buffer))?;
    let exec = ropbuf::ExecuteResponse::parse(&resp.body)?;
    println!("  X-ResponseCode : {}", resp.response_code);
    println!("  StatusCode     : 0x{:08X}", exec.status_code);
    println!(
        "  ErrorCode      : 0x{:08X} ({})",
        exec.error_code,
        rop::ec_name(exec.error_code)
    );
    if !exec.ok() {
        return Err("Execute failed".into());
    }

    let (rops, handles) = ropbuf::RopBuffer::parse(&exec.rop_buffer)?;
    println!(
        "  response ROPs  : {} bytes; handle table: {:08X?}",
        rops.len(),
        handles
    );
    println!("  NoCompression|NoXorMagic honoured: yes (payload parsed unobfuscated)");

    let mut r = cursor::Reader::new(&rops);
    match rop::decode_one(&mut r)? {
        RopResponse::Logon(logon) => {
            println!(
                "\n  RopLogon OK — replica id {}, mailbox GUID {:02x?}",
                logon.replica_id,
                &logon.mailbox_guid[..4]
            );
            println!(
                "  13 FolderIds from the logon response (no RopGetReceiveFolder, no EntryIDs):"
            );
            for (i, name) in FOLDER_NAMES.iter().enumerate() {
                println!(
                    "    [{i:2}] {name:<16} 0x{:016X}",
                    logon.folder(i).unwrap_or(0)
                );
            }
            println!("\nCP3 measurements:");
            println!(
                "  RopBuffer framing accepted first try (RopSize, handle table, RPC_HEADER_EXT)"
            );
            println!("  NoCompression|NoXorMagic honoured  -> no LZ77/DIRECT2, no 0xA5 XOR needed");
            println!("  AuxiliaryBufferSize=0 accepted     -> MS-OXCRPC aux layer deferred");
            println!("  HTTP round trips so far: 2 (Connect, Execute)");
            println!("  ROPs implemented: 1 of 6; OXCDATA property types: 0 of <=7");
            let logon_handle = handles.first().copied().unwrap_or(ropbuf::HANDLE_NONE);
            Ok((logon, logon_handle))
        }
        RopResponse::Failed {
            rop_id,
            return_value,
        } => Err(format!(
            "ROP 0x{rop_id:02X} failed: {} (0x{return_value:08X})",
            rop::ec_name(return_value)
        )
        .into()),
        other => Err(format!("unexpected response: {other:?}").into()),
    }
}

/// CP4: the whole folder/table chain in **one** `Execute`, to test whether the
/// server honours in-buffer handle chaining. Four ROPs, three handle slots.
fn do_rows(
    session: &mut http::Session,
    logon: &(rop::LogonResponse, u32),
    which: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use proptag::{CONTENTS_COLUMNS, HIERARCHY_COLUMNS};
    use table::{H_FOLDER, H_LOGON, H_TABLE};

    let (logon_resp, logon_handle) = logon;
    let hierarchy = which != "contents";
    let (slot, columns): (usize, &[u32]) = if hierarchy {
        (rop::FOLDER_IPM_SUBTREE, &HIERARCHY_COLUMNS)
    } else {
        (rop::FOLDER_INBOX, &CONTENTS_COLUMNS)
    };
    let folder_id = logon_resp
        .folder(slot)
        .ok_or("logon returned no such folder slot")?;

    println!(
        "\nExecute #2: OpenFolder -> Get{}Table -> SetColumns -> QueryRows",
        if hierarchy { "Hierarchy" } else { "Contents" }
    );
    println!(
        "  folder         : [{slot}] {} = 0x{folder_id:016X}",
        rop::FOLDER_NAMES[slot]
    );
    println!(
        "  columns        : {}",
        columns
            .iter()
            .map(|t| proptag::tag_name(*t))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut buf = ropbuf::RopBuffer::new();
    buf.push_rop(&table::open_folder_request(H_LOGON, H_FOLDER, folder_id));
    buf.push_rop(&if hierarchy {
        table::hierarchy_table_request(H_FOLDER, H_TABLE)
    } else {
        table::contents_table_request(H_FOLDER, H_TABLE)
    });
    buf.push_rop(&table::set_columns_request(H_TABLE, columns));
    buf.push_rop(&table::query_rows_request(H_TABLE, 25));
    // Slot 0 carries the handle the logon produced; 1 and 2 are filled in by
    // the server as it walks the list.
    buf.set_handle(H_LOGON, *logon_handle);
    buf.set_handle(H_FOLDER, ropbuf::HANDLE_NONE);
    buf.set_handle(H_TABLE, ropbuf::HANDLE_NONE);

    let rop_buffer = buf.serialize();
    println!(
        "  RopBuffer      : {} bytes (4 ROPs, 3 handle slots, ONE round trip)",
        rop_buffer.len()
    );

    let resp = session.post("Execute", ropbuf::execute_request(&rop_buffer))?;
    let exec = ropbuf::ExecuteResponse::parse(&resp.body)?;
    if !exec.ok() {
        return Err(format!("Execute failed: 0x{:08X}", exec.error_code).into());
    }
    let (rops, handles) = ropbuf::RopBuffer::parse(&exec.rop_buffer)?;
    println!("  handle table   : {handles:08X?}");

    // Decode by RopId off the stream — never positionally.
    let mut r = cursor::Reader::new(&rops);
    let mut rows_printed = 0usize;
    // The three things a spec reading cannot settle, so the run has to report
    // them: which row form the server chose, whether a long string came back
    // truncated or as an error, and how many columns errored.
    let mut row_forms: Option<(usize, usize)> = None;
    let mut longest_string: Option<usize> = None;
    let mut error_values = 0usize;
    while !r.is_empty() {
        let rop_id = r.u8()?;
        let _handle_index = r.u8()?;
        let rv = r.u32()?;
        if rv != 0 {
            println!("  ROP 0x{rop_id:02X} -> {} (0x{rv:08X})", rop::ec_name(rv));
            break;
        }
        match rop_id {
            rop::ROP_OPEN_FOLDER => {
                let ghosted = table::read_open_folder(&mut r)?;
                println!("  ROP 0x02 OpenFolder        OK (ghosted: {ghosted})");
            }
            rop::ROP_GET_HIERARCHY_TABLE | rop::ROP_GET_CONTENTS_TABLE => {
                let n = table::read_table_row_count(&mut r)?;
                println!("  ROP 0x{rop_id:02X} GetTable          OK ({n} rows in table)");
            }
            rop::ROP_SET_COLUMNS => {
                let status = table::read_set_columns(&mut r)?;
                println!("  ROP 0x12 SetColumns        OK (TableStatus {status})");
            }
            rop::ROP_QUERY_ROWS => {
                let q = table::read_query_rows(&mut r, columns)?;
                println!(
                    "  ROP 0x15 QueryRows         OK (origin {}, {} rows)\n",
                    q.origin,
                    q.rows.len()
                );
                for row in &q.rows {
                    let cells: Vec<String> = row
                        .values
                        .iter()
                        .zip(columns)
                        .map(|(v, t)| format!("{}={v}", proptag::tag_name(*t)))
                        .collect();
                    println!(
                        "    [{}] {}",
                        if row.flagged { "Flagged" } else { "Standard" },
                        cells.join("  ")
                    );
                    rows_printed += 1;
                }
                row_forms = Some(q.form_counts());
                longest_string = q
                    .rows
                    .iter()
                    .flat_map(|row| &row.values)
                    .filter_map(|v| match v {
                        proptag::PropValue::Str(s) => Some(s.chars().count()),
                        _ => None,
                    })
                    .max();
                error_values = q
                    .rows
                    .iter()
                    .flat_map(|row| &row.values)
                    .filter(|v| matches!(v, proptag::PropValue::Error(_)))
                    .count();
            }
            other => {
                println!("  unexpected ROP 0x{other:02X}");
                break;
            }
        }
    }

    println!("\nCP4 measurements:");
    println!("  rows decoded from a real server : {rows_printed}");
    if let Some((standard, flagged)) = row_forms {
        println!("  row form (server's choice)      : {standard} Standard, {flagged} Flagged");
    }
    if let Some(longest) = longest_string {
        println!("  longest string value returned   : {longest} chars");
    }
    println!("  columns returned as flag 0xA    : {error_values}");
    println!(
        "  in-buffer handle chaining       : {}",
        if rows_printed > 0 {
            "WORKS (4 ROPs, 1 Execute)"
        } else {
            "see above"
        }
    );
    println!("  HTTP round trips to first row   : 3 (Connect, Execute#1 logon, Execute#2 chain)");
    println!("  Execute calls                   : 2");
    println!("  ROPs implemented                : 6 of 6");
    println!("  OXCDATA property types          : 6");
    Ok(())
}
